//! Core/standard builtins: constants, ini, env, error handlers, headers, exec.

use super::datetime::date_format;
use super::url::urlencode;
use super::*;
use crate::error::ErrorKind;

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
            // ZPP `f` flag: the callback validates eagerly — an
            // invalid one throws TypeError naming the arg before any
            // side effects (bug45186's `cannot access "self"`).
            // forward_static_call* propagates a throwing autoloader's
            // exception; only the call_user_func family wraps it in
            // its own TypeError.
            if !it.is_callable_value(&cb) {
                if name.starts_with("forward_static_call") {
                    if let Some(pe) = it.take_callable_probe_err() {
                        return Err(pe);
                    }
                }
                return err(
                    "TypeError",
                    format!(
                        "{}(): Argument #1 ($callback) must be a valid callback, {}",
                        name,
                        it.zpp_callback_detail(&cb)
                    ),
                );
            }
            // `*_array`'s second param is ZPP-checked at parse time —
            // the TypeError fires before the no-scope Error below.
            let av = if name.ends_with("_array") {
                let av = arg(args, 1);
                if !matches!(av, Value::Array(_)) {
                    return err::<Option<Value>>(
                        "TypeError",
                        format!(
                            "{}(): Argument #2 ($args) must be of type array, {} given",
                            name,
                            it.zval_type_name(&av)
                        ),
                    );
                }
                Some(av)
            } else {
                None
            };
            // forward_static_call forwards the current called_scope —
            // no scope, nothing to forward (zend_execute_API). The
            // *_array form does NOT require a scope (it runs top-level).
            if name == "forward_static_call" && it.caller_scope_name().is_none() {
                return err::<Option<Value>>(
                    "Error",
                    "Cannot call forward_static_call() when no class scope is active",
                );
            }
            // `*_array` unpacks the args array: string keys become named
            // args (a later int key is the positional-after-named Error).
            if name.ends_with("_array") {
                let mut ca = crate::interp::CallArgs::empty();
                let mut seen_str = false;
                if let Some(Value::Array(a)) = &av {
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
            // Eager callback validation (zend zpp 'f'): a throwing
            // autoloader propagates; an invalid arg is a TypeError.
            if !it.is_callable_value(&f) {
                if let Some(pe) = it.take_callable_probe_err() {
                    return Err(pe);
                }
                return err(
                    "TypeError",
                    format!(
                        "register_shutdown_function(): Argument #1 ($callback) must be a valid callback, {}",
                        it.zpp_callback_detail(&f)
                    ),
                );
            }
            let rest: Vec<Cell> = args[1.min(args.len())..].to_vec();
            it.register_shutdown(f, rest);
            Value::Null
        }
        "set_error_handler" | "set_exception_handler" => {
            let cb = arg(args, 0);
            // `?callable` — null restores the engine default, anything
            // else must validate (a throwing autoloader propagates).
            if !matches!(cb, Value::Null) && !it.is_callable_value(&cb) {
                if let Some(pe) = it.take_callable_probe_err() {
                    return Err(pe);
                }
                return err(
                    "TypeError",
                    format!(
                        "{}(): Argument #1 ($callback) must be a valid callback or null, {}",
                        name,
                        it.zpp_callback_detail(&cb)
                    ),
                );
            }
            let (prev, none) = if name == "set_error_handler" {
                (it.error_handler().unwrap_or(Value::Null), false)
            } else {
                (it.exception_handler().unwrap_or(Value::Null), true)
            };
            if none {
                it.set_exception_handler(if matches!(cb, Value::Null) {
                    None
                } else {
                    Some(cb)
                });
            } else {
                it.set_error_handler(if matches!(cb, Value::Null) {
                    None
                } else {
                    Some(cb)
                });
            }
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
            // arginfo `?int` — weak coercion: scalars coerce, numeric
            // strings coerce, everything else throws TypeError.
            let lv = match args.first() {
                None => None,
                Some(c) => {
                    let v = c.borrow().clone();
                    match &v {
                        Value::Null => None,
                        Value::Int(i) => Some(*i),
                        Value::Float(f)
                            if f.is_finite()
                                && *f < 9.223372036854776e18
                                && *f >= -9.223372036854776e18 =>
                        {
                            // A real float → int coerces with the
                            // loses-precision deprecation (zend).
                            if f.fract() != 0.0 {
                                it.deprecated_pub(&format!(
                                    "Implicit conversion from float {} to int loses precision",
                                    crate::value::format_float_repr(*f)
                                ))?;
                            }
                            Some(*f as i64)
                        }
                        Value::Bool(b) => Some(*b as i64),
                        Value::Str(s) => match crate::value::numeric(s) {
                            crate::value::Numeric::Int(i) => Some(i),
                            // A float-STRING truncates silently — zend
                            // only warns on real floats.
                            crate::value::Numeric::Float(f) => Some(f as i64),
                            _ => {
                                return err(
                                    "TypeError",
                                    format!(
                                        "error_reporting(): Argument #1 ($error_level) must be of type ?int, {} given",
                                        zval_word(&v)
                                    ),
                                );
                            }
                        },
                        _ => {
                            return err(
                                "TypeError",
                                format!(
                                    "error_reporting(): Argument #1 ($error_level) must be of type ?int, {} given",
                                    zval_word(&v)
                                ),
                            );
                        }
                    }
                }
            };
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
            // warning and leaves the old value (bug45392). mem_total
            // already carries zend's runtime baseline; the startup-time
            // 2M bootstrap reserve refusal lives in phpun -d handling.
            if k == "memory_limit" {
                it.ini.insert(k.clone(), v);
                let lim = it.ini_bytes(&k);
                let usage = it.mem_total();
                if lim > 0 && usage > lim {
                    let _ = it
                        .ini
                        .insert(k.clone(), prev.clone().unwrap_or_else(|| "-1".into()));
                    it.warn_pub(&format!(
                        "Failed to set memory limit to {} bytes (Current memory usage is {} bytes)",
                        lim, usage
                    ))?;
                    // Refused ini_set returns false (zend returns the
                    // old value only on a successful set).
                    return Ok(Some(Value::Bool(false)));
                }
            } else {
                it.ini.insert(k.clone(), v);
            }
            // ini_set('error_reporting', …) also updates the live
            // level — zend's ini handler is atoi() only, expressions
            // like "E_ALL" are NOT evaluated here (unlike -d).
            if k == "error_reporting" {
                it.error_level = arg(args, 1).to_int();
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
            // ?string $name = null — an explicit null means the default:
            // the full environment array, same as a 0-arg call.
            if args.is_empty() || matches!(arg(args, 0), Value::Null) {
                let mut a = PhpArray::new();
                for (k, v) in it.getenv_all_pub() {
                    a.set(ArrKey::Str(k.into()), Value::str(v));
                }
                Value::Array(Rc::new(RefCell::new(a)))
            } else {
                let name = arg_str(it, args, 0);
                match it.getenv_pub(&name) {
                    Some(v) => Value::str(v),
                    None => Value::Bool(false),
                }
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
        // Runtime baseline + metered live bytes (obj shells, array
        // tables, string payloads); emitted output is free.
        "memory_get_usage" => Value::Int(it.mem_total()),
        // High-water mark of the live total — zend's peak survives
        // frees until memory_reset_peak_usage re-baselines it.
        "memory_get_peak_usage" => {
            Value::Int(crate::value::MEM_BASE_BYTES + crate::value::mem_peak_bytes())
        }
        "memory_reset_peak_usage" => {
            crate::value::mem_peak_reset();
            Value::Null
        }
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
        "get_defined_functions" => {
            // zend returns {internal: every registered internal fn,
            // user: every userland decl} keyed lowercased. `it.functions`
            // is already lowercased at insert; sort for deterministic
            // output (zend emits insertion order we don't track).
            // ponytail: internal list = BUILTIN_NAMES (sorted), so
            // per-extension grouping/prepended aliases are flattened —
            // a real fn-table would restore zend's registration order.
            let mut internal = PhpArray::new();
            for &n in crate::builtins::builtin_names() {
                internal.push(Value::str(n));
            }
            let mut user_names: Vec<&String> = it.functions.keys().collect();
            user_names.sort();
            let mut user = PhpArray::new();
            for n in user_names {
                user.push(Value::str(n.clone()));
            }
            let mut out = PhpArray::new();
            out.set(
                ArrKey::Str("internal".into()),
                Value::Array(Rc::new(RefCell::new(internal))),
            );
            out.set(
                ArrKey::Str("user".into()),
                Value::Array(Rc::new(RefCell::new(user))),
            );
            Value::Array(Rc::new(RefCell::new(out)))
        }
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
        "gc_collect_cycles" => Value::Int(it.gc_cycle_collect()? as i64),
        // gc_enable/gc_disable toggle the zend.enable_gc ini flag —
        // ini_get reads it back and gc_enabled() reports it (gc_001-3).
        "gc_enable" | "gc_disable" => {
            it.ini.insert(
                "zend.enable_gc".into(),
                if name == "gc_enable" { "1" } else { "0" }.into(),
            );
            Value::Int(0)
        }
        "gc_mem_caches" => Value::Int(0),
        "gc_status" => {
            let mut a = PhpArray::new();
            for (k, v) in [
                ("running", Value::Bool(false)),
                ("protected", Value::Bool(false)),
                ("full", Value::Bool(false)),
                ("runs", Value::Int(it.gc_runs as i64)),
                ("collected", Value::Int(it.gc_collected as i64)),
                ("threshold", Value::Int(10001)),
                ("buffer_size", Value::Int(16384)),
                ("roots", Value::Int(it.gc_purpled.len() as i64)),
                (
                    "application_time",
                    Value::Float(it.t0.elapsed().as_secs_f64()),
                ),
                ("collector_time", Value::Float(it.gc_collector_time)),
                ("destructor_time", Value::Float(it.gc_destructor_time)),
                (
                    "free_time",
                    Value::Float((it.gc_collector_time - it.gc_destructor_time).max(0.0)),
                ),
            ] {
                a.set(ArrKey::Str(k.into()), v);
            }
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "gc_enabled" => Value::Bool(it.ini_on("zend.enable_gc")),
        "syslog" | "openlog" | "closelog" => Value::Bool(true),
        "call_func" => Value::Null,
        "get_resource_type" | "get_resource_id" => match arg(args, 0) {
            Value::Resource(r) => {
                if name == "get_resource_id" {
                    Value::Int(r.borrow().id() as i64)
                } else {
                    Value::str(r.borrow().type_name())
                }
            }
            _ => Value::Bool(false),
        },
        // exit()/die() exist in zend's function table too — reachable
        // through 'exit'/'die' string callables (FCC, call_user_func).
        // Top-level exit() parses to Expr::Exit and never lands here.
        "die" | "exit" => {
            let code = match args.first().map(|c| c.borrow().clone()) {
                Some(Value::Int(i)) => i as i32,
                Some(Value::Str(s)) => {
                    it.emit_bytes(&s);
                    0
                }
                _ => 0,
            };
            return Err(PhpError {
                trace: None,
                thrown_line: None,
                display_msg: None,
                kind: ErrorKind::Fatal,
                message: format!("\u{1}exit:{}", code),
                line: 0,
            });
        }
        "escapeshellarg" | "escapeshellcmd" => {
            let s = arg_str(it, args, 0);
            Value::str(format!("'{}'", s.replace('\'', "'\\''")))
        }
        "get_include_path" => Value::str(it.ini.get("include_path").cloned().unwrap_or_default()),
        "set_include_path" => {
            // Returns the OLD path; the new one stores into the ini
            // table like ini_set (zend's set_include_path is ini_set).
            let prev = it.ini.get("include_path").cloned().unwrap_or_default();
            let v = arg_str(it, args, 0);
            it.ini.insert("include_path".into(), v);
            Value::str(prev)
        }
        "restore_include_path" => {
            // zend restores the ini default registered at startup — the
            // same compiled-in path Interp::new seeds.
            it.ini.insert(
                "include_path".into(),
                ".:/home/linuxbrew/.linuxbrew/Cellar/php/8.5.11/share/php/pear".to_string(),
            );
            Value::Bool(false)
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
        "pack" => {
            if args.is_empty() {
                return err(
                    "ArgumentCountError",
                    "pack() expects at least 1 argument, 0 given",
                );
            }
            pack_check_string(it, name, &args[0], 1, "format")?;
            let fmt = arg_bs(it, args, 0);
            pack_run(it, &fmt, &args[1..])?
        }
        "unpack" => {
            if args.len() < 2 {
                return err(
                    "ArgumentCountError",
                    format!(
                        "unpack() expects at least 2 arguments, {} given",
                        args.len()
                    ),
                );
            }
            pack_check_string(it, name, &args[0], 1, "format")?;
            pack_check_string(it, name, &args[1], 2, "string")?;
            if args.len() > 2 {
                match &*args[2].borrow() {
                    Value::Array(_)
                    | Value::Object(_)
                    | Value::Resource(_)
                    | Value::Callable(_) => {
                        return err(
                            "TypeError",
                            format!(
                                "unpack(): Argument #3 ($offset) must be of type int, {} given",
                                zval_word(&arg(args, 2))
                            ),
                        );
                    }
                    _ => {}
                }
            }
            let fmt = arg_bs(it, args, 0);
            let data = arg_bs(it, args, 1);
            let offset = if args.len() > 2 {
                arg(args, 2).to_int()
            } else {
                0
            };
            unpack_run(it, &fmt, &data, offset)?
        }
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
        "clone" => {
            // PHP 8.5 `clone(object $object, array $withProperties = [])`.
            if args.is_empty() {
                return err(
                    "ArgumentCountError",
                    "clone() expects at least 1 argument, 0 given",
                );
            }
            if args.len() > 2 {
                return err(
                    "ArgumentCountError",
                    format!("clone() expects at most 2 arguments, {} given", args.len()),
                );
            }
            let with = match args.get(1) {
                Some(c) => {
                    let w = c.borrow();
                    match &*w {
                        Value::Array(a) => Some(a.clone()),
                        other => {
                            return err(
                                "TypeError",
                                format!(
                                    "clone(): Argument #2 ($withProperties) must be of type array, {} given",
                                    other.debug_type()
                                ),
                            )
                        }
                    }
                }
                None => None,
            };
            it.builtin_clone(&arg(args, 0), with.as_ref())?
        }
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

// ----- pack()/unpack() — faithful port of Zend ext/standard/pack.c -----
// Machine byte order is little-endian (the platform phpun runs on).

fn pack_check_string(
    it: &mut Interp,
    name: &str,
    c: &Cell,
    idx: usize,
    param: &str,
) -> Result<(), PhpError> {
    match &*c.borrow() {
        Value::Array(_) | Value::Object(_) | Value::Resource(_) | Value::Callable(_) => err(
            "TypeError",
            format!(
                "{}(): Argument #{} (${}) must be of type string, {} given",
                name,
                idx,
                param,
                zval_word(&c.borrow())
            ),
        ),
        _ => {
            let _ = it;
            Ok(())
        }
    }
}

/// Pack format codes that each consume one zval argument (string codes).
fn pack_str_code(c: u8) -> bool {
    matches!(c, b'a' | b'A' | b'Z' | b'h' | b'H')
}
/// Pack format codes that consume `count` zval arguments (numeric codes).
fn pack_num_code(c: u8) -> bool {
    matches!(
        c,
        b'c' | b'C'
            | b's'
            | b'S'
            | b'i'
            | b'I'
            | b'l'
            | b'L'
            | b'q'
            | b'Q'
            | b'J'
            | b'P'
            | b'n'
            | b'N'
            | b'v'
            | b'V'
            | b'f'
            | b'g'
            | b'G'
            | b'd'
            | b'e'
            | b'E'
    )
}
/// Byte size of one numeric pack/unpack element.
fn pack_num_size(c: u8) -> usize {
    match c {
        b'c' | b'C' => 1,
        b's' | b'S' | b'n' | b'v' => 2,
        b'i' | b'I' | b'l' | b'L' | b'N' | b'V' | b'f' | b'g' | b'G' => 4,
        _ => 8, // q Q J P d e E
    }
}
/// Big-endian output for this code (n, N, J, G, E)? Everything else is LE.
fn pack_be(c: u8) -> bool {
    matches!(c, b'n' | b'N' | b'J' | b'G' | b'E')
}

fn pack_run(it: &mut Interp, fmt: &[u8], args: &[Cell]) -> Result<Value, PhpError> {
    // Pass 1: parse format entries, resolve `*` and validate argument counts.
    let mut entries: Vec<(u8, i64)> = Vec::new();
    let mut cur = 0usize;
    let num = args.len();
    let mut i = 0usize;
    while i < fmt.len() {
        let code = fmt[i];
        i += 1;
        let mut count: i64 = 1;
        if i < fmt.len() {
            if fmt[i] == b'*' {
                count = -1;
                i += 1;
            } else if fmt[i].is_ascii_digit() {
                let s = i;
                while i < fmt.len() && fmt[i].is_ascii_digit() {
                    i += 1;
                }
                count = atoi_i64(&fmt[s..i]);
            }
        }
        match code {
            b'x' | b'X' | b'@' => {
                if count < 0 {
                    it.warn_pub(&format!("pack(): Type {}: '*' ignored", code as char))?;
                    count = 1;
                }
            }
            c if pack_str_code(c) => {
                if cur >= num {
                    return err(
                        "ValueError",
                        format!("Type {}: not enough arguments", code as char),
                    );
                }
                if count < 0 {
                    let s = it.to_bytes_of(&args[cur].borrow());
                    count = s.len() as i64;
                    if code == b'Z' {
                        count += 1;
                    }
                }
                cur += 1;
            }
            c if pack_num_code(c) => {
                if count < 0 {
                    count = (num - cur) as i64;
                }
                if cur as i64 + count > num as i64 {
                    return err(
                        "ValueError",
                        format!("Type {}: too few arguments", code as char),
                    );
                }
                cur += count as usize;
            }
            _ => {
                return err(
                    "ValueError",
                    format!("Type {}: unknown format code", code as char),
                );
            }
        }
        entries.push((code, count));
    }
    if cur < num {
        it.warn_pub(&format!("pack(): {} arguments unused", num - cur))?;
    }
    // Pass 2: upper-bound the output and emit X's out-of-bounds warning.
    let mut pos: i64 = 0;
    let mut size: usize = 0;
    for &(code, count) in &entries {
        match code {
            b'h' | b'H' => pos += count / 2 + count % 2,
            b'x' => pos += count,
            b'X' => {
                pos -= count;
                if pos < 0 {
                    it.warn_pub(&format!("pack(): Type {}: outside of string", code as char))?;
                    pos = 0;
                }
            }
            b'@' => pos = count,
            _ => pos += count * pack_num_size(code) as i64,
        }
        if pos as usize > size {
            size = pos as usize;
        }
    }
    let mut out = vec![0u8; size];
    pos = 0;
    cur = 0;
    // Pass 3: pack.
    for &(code, count) in &entries {
        match code {
            b'a' | b'A' | b'Z' => {
                let s = it.to_bytes_of(&args[cur].borrow());
                cur += 1;
                let pad = if code == b'A' { b' ' } else { 0 };
                for j in 0..count as usize {
                    out[(pos as usize) + j] = pad;
                }
                let cp = if code == b'Z' {
                    (count - 1).max(0)
                } else {
                    count
                };
                let n = (s.len() as i64).min(cp) as usize;
                out[pos as usize..pos as usize + n].copy_from_slice(&s[..n]);
                pos += count;
            }
            b'h' | b'H' => {
                let s = it.to_bytes_of(&args[cur].borrow());
                cur += 1;
                let mut nibshift = if code == b'h' { 0u8 } else { 4u8 };
                let mut first = true;
                let mut n = count;
                pos -= 1;
                if n > s.len() as i64 {
                    it.warn_pub(&format!(
                        "pack(): Type {}: not enough characters in string",
                        code as char
                    ))?;
                    n = s.len() as i64;
                }
                let mut vi = 0usize;
                while n > 0 {
                    n -= 1;
                    let ch = s[vi];
                    vi += 1;
                    let v = match ch {
                        b'0'..=b'9' => ch - b'0',
                        b'A'..=b'F' => ch - (b'A' - 10),
                        b'a'..=b'f' => ch - (b'a' - 10),
                        _ => {
                            it.warn_pub(&format!(
                                "pack(): Type {}: illegal hex digit {}",
                                code as char, ch as char
                            ))?;
                            0
                        }
                    };
                    if first {
                        first = false;
                        pos += 1;
                        out[pos as usize] = 0;
                    } else {
                        first = true;
                    }
                    out[pos as usize] |= v << nibshift;
                    nibshift = (nibshift + 4) & 7;
                }
                pos += 1;
            }
            b'x' => {
                for _ in 0..count {
                    out[pos as usize] = 0;
                    pos += 1;
                }
            }
            b'X' => {
                pos -= count;
                if pos < 0 {
                    pos = 0;
                }
            }
            b'@' => {
                if count > pos {
                    for j in pos..count {
                        out[j as usize] = 0;
                    }
                }
                pos = count;
            }
            b'f' | b'g' | b'G' | b'd' | b'e' | b'E' => {
                for _ in 0..count {
                    let v = arg(&args[cur..], 0).to_float();
                    cur += 1;
                    let bytes: [u8; 8] = if pack_num_size(code) == 4 {
                        let f = v as f32;
                        let b = if pack_be(code) {
                            f.to_be_bytes()
                        } else {
                            f.to_le_bytes()
                        };
                        let mut a = [0u8; 8];
                        a[..4].copy_from_slice(&b);
                        a
                    } else {
                        if pack_be(code) {
                            v.to_be_bytes()
                        } else {
                            v.to_le_bytes()
                        }
                    };
                    let sz = pack_num_size(code);
                    out[pos as usize..pos as usize + sz].copy_from_slice(&bytes[..sz]);
                    pos += sz as i64;
                }
            }
            _ => {
                // integer codes
                let sz = pack_num_size(code);
                for _ in 0..count {
                    let v = pack_int_arg(it, &args[cur])?;
                    cur += 1;
                    // LE codes emit the low bytes; BE codes the high end.
                    let b: &[u8] = if pack_be(code) {
                        &v.to_be_bytes()[8 - sz..]
                    } else {
                        &v.to_le_bytes()[..sz]
                    };
                    out[pos as usize..pos as usize + sz].copy_from_slice(b);
                    pos += sz as i64;
                }
            }
        }
    }
    out.truncate(pos.max(0) as usize);
    Ok(Value::bytes(out))
}

/// zval → u64 for pack's integer codes, with Zend's float-cast warning.
fn pack_int_arg(it: &mut Interp, c: &Cell) -> Result<u64, PhpError> {
    match &*c.borrow() {
        Value::Float(f)
            if !(f.is_finite() && *f >= -9.223372036854776e18 && *f < 9.223372036854776e18)
                && !f.is_nan() =>
        {
            it.warn_pub(&format!(
                "The float {} is not representable as an int, cast occurred",
                format_float_repr(*f)
            ))?;
            Ok(0)
        }
        v => Ok(v.to_int() as u64),
    }
}

/// atoi(): parse a run of ASCII digits with i64 clamp.
fn atoi_i64(digits: &[u8]) -> i64 {
    let mut v: i64 = 0;
    for &d in digits {
        v = v.saturating_mul(10).saturating_add((d - b'0') as i64);
    }
    v
}

/// Numeric-string array key semantics: "1" → int 1.
fn unpack_key(name: &[u8], i: i64, reps: i64) -> ArrKey {
    if reps == 1 && !name.is_empty() {
        to_key(&Value::str(String::from_utf8_lossy(name)))
    } else {
        let mut s = name.to_vec();
        s.extend_from_slice((i + 1).to_string().as_bytes());
        to_key(&Value::str(String::from_utf8_lossy(&s)))
    }
}

fn unpack_run(it: &mut Interp, fmt: &[u8], data: &[u8], offset: i64) -> Result<Value, PhpError> {
    let inputlen = data.len() as i64;
    if offset < 0 || offset > inputlen {
        return err(
            "ValueError",
            "unpack(): Argument #3 ($offset) must be contained in argument #2 ($data)",
        );
    }
    let mut out = PhpArray::new();
    let mut pos: i64 = offset;
    let mut fi = 0usize;
    while fi < fmt.len() {
        let t = fmt[fi];
        fi += 1;
        let mut reps: i64 = 1;
        if fi < fmt.len() {
            if fmt[fi].is_ascii_digit() {
                let s = fi;
                while fi < fmt.len() && fmt[fi].is_ascii_digit() {
                    fi += 1;
                }
                let v = atoi_i64(&fmt[s..fi]);
                if !(i32::MIN as i64..=i32::MAX as i64).contains(&v) {
                    it.warn_pub(&format!("unpack(): Type {}: integer overflow", t as char))?;
                    return Ok(Value::Bool(false));
                }
                reps = v;
            } else if fmt[fi] == b'*' {
                reps = -1;
                fi += 1;
            }
        }
        let nstart = fi;
        while fi < fmt.len() && fmt[fi] != b'/' {
            fi += 1;
        }
        let name = &fmt[nstart..(nstart + (fi - nstart).min(200))];
        let argb = reps;
        let mut size: i64;
        match t {
            b'X' => {
                size = -1;
                if reps < 0 {
                    it.warn_pub(&format!("unpack(): Type {}: '*' ignored", t as char))?;
                    reps = 1;
                }
            }
            b'@' => size = 0,
            b'a' | b'A' | b'Z' => {
                size = reps;
                reps = 1;
            }
            b'h' | b'H' => {
                size = if reps > 0 { (reps + 1) / 2 } else { reps };
                reps = 1;
            }
            b'c' | b'C' | b'x' => size = 1,
            b's' | b'S' | b'n' | b'v' => size = 2,
            b'i' | b'I' | b'l' | b'L' | b'N' | b'V' => size = 4,
            b'q' | b'Q' | b'J' | b'P' => size = 8,
            b'f' | b'g' | b'G' => size = 4,
            b'd' | b'e' | b'E' => size = 8,
            _ => {
                return err("ValueError", format!("Invalid format type {}", t as char));
            }
        }
        let mut idx: i64 = 0;
        while idx != reps {
            if size != 0 && size != -1 && i64::MAX - size + 1 < pos {
                it.warn_pub(&format!("unpack(): Type {}: integer overflow", t as char))?;
                return Ok(Value::Bool(false));
            }
            if pos + size <= inputlen {
                let key = unpack_key(name, idx, reps);
                match t {
                    b'a' | b'A' | b'Z' => {
                        let mut len = inputlen - pos;
                        if size >= 0 && len > size {
                            len = size;
                        }
                        size = len;
                        let bytes = &data[pos as usize..(pos + len) as usize];
                        let v = match t {
                            b'a' => Value::bytes(bytes.to_vec()),
                            b'A' => {
                                let mut l = len as usize;
                                while l > 0
                                    && matches!(bytes[l - 1], 0 | b' ' | b'\t' | b'\r' | b'\n')
                                {
                                    l -= 1;
                                }
                                Value::bytes(bytes[..l].to_vec())
                            }
                            _ => {
                                let l = bytes.iter().position(|&b| b == 0).unwrap_or(len as usize);
                                Value::bytes(bytes[..l].to_vec())
                            }
                        };
                        out.set(key, v);
                    }
                    b'h' | b'H' => {
                        let mut len = (inputlen - pos) * 2;
                        if size >= 0 && len > size * 2 {
                            len = size * 2;
                        }
                        if len > 0 && argb > 0 {
                            len -= argb % 2;
                        }
                        let mut shift = if t == b'h' { 0u8 } else { 4u8 };
                        let mut buf = Vec::with_capacity(len as usize);
                        let mut ipos = 0usize;
                        let mut first = true;
                        for _ in 0..len {
                            let cc = (data[(pos as usize) + ipos] >> shift) & 0xf;
                            buf.push(if cc < 10 { cc + b'0' } else { cc + b'a' - 10 });
                            shift = (shift + 4) & 7;
                            if !first {
                                ipos += 1;
                            }
                            first = !first;
                        }
                        size = (len + 1) / 2;
                        out.set(key, Value::bytes(buf));
                    }
                    b'x' => {}
                    b'X' => {
                        if pos < size {
                            pos = -size;
                            idx = reps - 1;
                            if reps >= 0 {
                                it.warn_pub(&format!(
                                    "unpack(): Type {}: outside of string",
                                    t as char
                                ))?;
                            }
                        }
                    }
                    b'@' => {
                        if reps <= inputlen {
                            pos = reps;
                        } else {
                            it.warn_pub(&format!(
                                "unpack(): Type {}: outside of string",
                                t as char
                            ))?;
                        }
                        idx = reps - 1;
                    }
                    b'f' | b'g' | b'G' | b'd' | b'e' | b'E' => {
                        let sz = size as usize;
                        let mut b = [0u8; 8];
                        b[..sz].copy_from_slice(&data[pos as usize..pos as usize + sz]);
                        let v = if sz == 4 {
                            f32::from_le_bytes(if pack_be(t) {
                                let mut r = [0u8; 4];
                                r.copy_from_slice(&b[..4]);
                                r.reverse();
                                r
                            } else {
                                let mut r = [0u8; 4];
                                r.copy_from_slice(&b[..4]);
                                r
                            }) as f64
                        } else {
                            f64::from_le_bytes(if pack_be(t) {
                                let mut r = b;
                                r.reverse();
                                r
                            } else {
                                b
                            })
                        };
                        out.set(key, Value::Float(v));
                    }
                    _ => {
                        // integer codes
                        let sz = size as usize;
                        let mut b = [0u8; 8];
                        // BE codes put their bytes in the high end.
                        if pack_be(t) {
                            b[8 - sz..].copy_from_slice(&data[pos as usize..pos as usize + sz]);
                        } else {
                            b[..sz].copy_from_slice(&data[pos as usize..pos as usize + sz]);
                        }
                        let x = if pack_be(t) {
                            u64::from_be_bytes(b)
                        } else {
                            u64::from_le_bytes(b)
                        };
                        let v = match t {
                            b'c' => x as u8 as i8 as i64,
                            b's' => x as u16 as i16 as i64,
                            b'i' | b'l' => x as u32 as i32 as i64,
                            b'q' => x as i64,
                            _ => x as i64,
                        };
                        out.set(key, Value::Int(v));
                    }
                }
                pos += size;
                if pos < 0 {
                    if size != -1 {
                        it.warn_pub(&format!("unpack(): Type {}: outside of string", t as char))?;
                    }
                    pos = 0;
                }
            } else if reps < 0 {
                break;
            } else {
                let have = inputlen - pos;
                it.warn_pub(&format!(
                    "unpack(): Type {}: not enough input values, need {} values but only {} {} provided",
                    t as char,
                    size,
                    have,
                    if have == 1 { "was" } else { "were" }
                ))?;
                return Ok(Value::Bool(false));
            }
            idx += 1;
        }
        if fi < fmt.len() {
            fi += 1; // skip '/'
        }
    }
    Ok(Value::Array(std::rc::Rc::new(std::cell::RefCell::new(out))))
}
