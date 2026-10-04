//! Filesystem/stream builtins: file fns, stream resources, stat, glob.

use super::crypto::base64_decode;
use super::*;

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
    Ok(Some(match name {
        // ----- filesystem/process -----
        "file_exists" => Value::Bool(std::path::Path::new(fs_path(&arg_str(it, args, 0))).exists()),
        "is_file" => Value::Bool(std::path::Path::new(fs_path(&arg_str(it, args, 0))).is_file()),
        "is_dir" => Value::Bool(std::path::Path::new(fs_path(&arg_str(it, args, 0))).is_dir()),
        "is_link" => Value::Bool(
            std::path::Path::new(fs_path(&arg_str(it, args, 0)))
                .symlink_metadata()
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false),
        ),
        "is_readable" => Value::Bool(std::path::Path::new(fs_path(&arg_str(it, args, 0))).exists()),
        "is_writable" | "is_writeable" => {
            Value::Bool(std::path::Path::new(fs_path(&arg_str(it, args, 0))).exists())
        }
        "is_executable" => {
            Value::Bool(std::path::Path::new(fs_path(&arg_str(it, args, 0))).exists())
        }
        "filesize" => match std::fs::metadata(fs_path(&arg_str(it, args, 0))) {
            Ok(m) => Value::Int(m.len() as i64),
            Err(_) => Value::Bool(false),
        },
        "filemtime" | "fileatime" | "filectime" => {
            match std::fs::metadata(fs_path(&arg_str(it, args, 0))) {
                Ok(m) => match m.modified() {
                    Ok(t) => Value::Int(
                        t.duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0),
                    ),
                    Err(_) => Value::Bool(false),
                },
                Err(_) => Value::Bool(false),
            }
        }
        "fileperms" => {
            use std::os::unix::fs::PermissionsExt;
            match std::fs::metadata(fs_path(&arg_str(it, args, 0))) {
                Ok(m) => Value::Int(m.permissions().mode() as i64),
                Err(_) => Value::Bool(false),
            }
        }
        "file_get_contents" => {
            let path = arg_str(it, args, 0);
            if path == "php://input" {
                Value::str(String::from_utf8_lossy(&it.php_input).into_owned())
            } else {
                match read_stream(&path) {
                    Ok(b) => Value::str(String::from_utf8_lossy(&b).into_owned()),
                    Err(e) => {
                        it.warn_pub(&format!(
                            "file_get_contents({}): Failed to open stream: {}",
                            path, e
                        ))?;
                        Value::Bool(false)
                    }
                }
            }
        }
        "file_put_contents" => {
            let path = arg_str(it, args, 0);
            let data = arg(args, 1).to_php_string();
            let append = arg(args, 2).to_int() & 8 != 0; // FILE_APPEND
            let r = if append {
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(fs_path(&path))
                    .and_then(|mut f| f.write_all(data.as_bytes()))
            } else {
                std::fs::write(fs_path(&path), data.as_bytes())
            };
            match r {
                Ok(_) => Value::Int(data.len() as i64),
                Err(e) => {
                    it.warn_pub(&format!(
                        "file_put_contents({}): Failed to open stream: {}",
                        path, e
                    ))?;
                    Value::Bool(false)
                }
            }
        }
        "unlink" => match std::fs::remove_file(fs_path(&arg_str(it, args, 0))) {
            Ok(_) => Value::Bool(true),
            Err(_) => Value::Bool(false),
        },
        "rename" => Value::Bool(
            std::fs::rename(
                fs_path(&arg_str(it, args, 0)),
                fs_path(&arg_str(it, args, 1)),
            )
            .is_ok(),
        ),
        "copy" => Value::Bool(
            std::fs::copy(
                fs_path(&arg_str(it, args, 0)),
                fs_path(&arg_str(it, args, 1)),
            )
            .is_ok(),
        ),
        "mkdir" => Value::Bool(std::fs::create_dir_all(fs_path(&arg_str(it, args, 0))).is_ok()),
        "rmdir" => Value::Bool(std::fs::remove_dir(fs_path(&arg_str(it, args, 0))).is_ok()),
        "basename" => {
            let p = arg_str(it, args, 0);
            let suffix = arg_str(it, args, 1);
            let b = std::path::Path::new(&p)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            Value::str(b.strip_suffix(&suffix).map(|s| s.to_string()).unwrap_or(b))
        }
        "dirname" | "pathinfo_dirname" => {
            let p = arg_str(it, args, 0);
            Value::str(
                std::path::Path::new(&p)
                    .parent()
                    .map(|d| {
                        let s = d.display().to_string();
                        if s.is_empty() {
                            ".".into()
                        } else {
                            s
                        }
                    })
                    .unwrap_or_else(|| ".".into()),
            )
        }
        "realpath" => match std::fs::canonicalize(fs_path(&arg_str(it, args, 0))) {
            Ok(p) => Value::str(p.display().to_string()),
            Err(_) => Value::Bool(false),
        },
        "pathinfo" => {
            let p = arg_str(it, args, 0);
            let path = std::path::Path::new(&p);
            let mut out = PhpArray::new();
            let dir = path
                .parent()
                .map(|d| d.display().to_string())
                .unwrap_or_else(|| ".".into());
            out.set(ArrKey::Str("dirname".into()), Value::str(dir));
            let file = path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.set(ArrKey::Str("basename".into()), Value::str(file.clone()));
            let stem = if ext.is_empty() {
                file.clone()
            } else {
                file.strip_suffix(&format!(".{}", ext))
                    .unwrap_or(&file)
                    .to_string()
            };
            if !ext.is_empty() {
                out.set(ArrKey::Str("extension".into()), Value::str(ext));
            }
            out.set(ArrKey::Str("filename".into()), Value::str(stem));
            if args.len() > 1 {
                let flag = arg(args, 1).to_int();
                // Bitflag selects one component; PATHINFO_ALL keeps the
                // full array. Multiple bits return first match per PHP.
                let v: Value = if flag == 15 {
                    Value::Array(Rc::new(RefCell::new(out)))
                } else if flag & 1 != 0 {
                    out.get(&ArrKey::Str("dirname".into()))
                        .unwrap_or(Value::str(""))
                } else if flag & 2 != 0 {
                    out.get(&ArrKey::Str("basename".into()))
                        .unwrap_or(Value::str(""))
                } else if flag & 4 != 0 {
                    out.get(&ArrKey::Str("extension".into()))
                        .unwrap_or(Value::str(""))
                } else if flag & 8 != 0 {
                    out.get(&ArrKey::Str("filename".into()))
                        .unwrap_or(Value::str(""))
                } else {
                    Value::Array(Rc::new(RefCell::new(out)))
                };
                return Ok(Some(v));
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "tempnam" => {
            let dir = arg_str(it, args, 0);
            let prefix = arg_str(it, args, 1);
            let name = format!("{}/{}{}", dir, prefix, std::process::id());
            let _ = std::fs::File::create(&name);
            Value::str(name)
        }
        "tmpfile" => {
            let name = std::env::temp_dir().join(format!("phpun-{}", std::process::id()));
            match std::fs::File::create(&name) {
                Ok(f) => {
                    let id = it.next_res_id();
                    Value::Resource(Rc::new(RefCell::new(PhpResource::File {
                        id,
                        file: f,
                        read: true,
                        write: true,
                        pos: 0,
                        eof: false,
                    })))
                }
                Err(_) => Value::Bool(false),
            }
        }
        "sys_get_temp_dir" => Value::str(std::env::temp_dir().display().to_string()),
        "getcwd" => match std::env::current_dir() {
            Ok(d) => Value::str(d.display().to_string()),
            Err(_) => Value::Bool(false),
        },
        "chdir" => Value::Bool(std::env::set_current_dir(arg_str(it, args, 0)).is_ok()),
        "glob" => {
            let pat = arg_str(it, args, 0);
            let flags = args.get(1).map(|c| c.borrow().to_int()).unwrap_or(0);
            const GLOB_ONLYDIR: i64 = 1 << 30;
            const GLOB_MARK: i64 = 8;
            const GLOB_NOCHECK: i64 = 16;
            let mut out = PhpArray::new();
            // minimal glob: only '*' and '?' in filename segments
            let dir = std::path::Path::new(&pat)
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            let fname = std::path::Path::new(&pat)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            let re = glob_to_regex(&fname);
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().into_owned();
                    if !re.is_match(&n) {
                        continue;
                    }
                    if flags & GLOB_ONLYDIR != 0 && !e.path().is_dir() {
                        continue;
                    }
                    let p = e.path();
                    let s = if std::path::Path::new(&pat).is_absolute() {
                        p.display().to_string()
                    } else {
                        let ds = dir.display().to_string();
                        if ds == "." {
                            n.clone()
                        } else {
                            format!("{}/{}", ds, n)
                        }
                    };
                    out.push(Value::str(if flags & GLOB_MARK != 0 {
                        format!("{}/", s)
                    } else {
                        s
                    }));
                }
            }
            // sorted like glob(3); empty -> pattern or false
            let mut v: Vec<Value> = out.iter().map(|(_, c)| c.borrow().clone()).collect();
            v.sort_by_key(|a| a.to_php_string());
            let mut sorted = PhpArray::new();
            for x in v {
                sorted.push(x);
            }
            if sorted.is_empty() && flags & GLOB_NOCHECK != 0 {
                sorted.push(Value::str(pat));
            }
            Value::Array(Rc::new(RefCell::new(sorted)))
        }
        "scandir" => {
            let mut out = PhpArray::new();
            if let Ok(rd) = std::fs::read_dir(fs_path(&arg_str(it, args, 0))) {
                out.push(Value::str("."));
                out.push(Value::str(".."));
                for e in rd.flatten() {
                    out.push(Value::str(e.file_name().to_string_lossy().into_owned()));
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "is_uploaded_file" => {
            let p = arg_str(it, args, 0);
            Value::Bool(it.uploads.iter().any(|u| u.display().to_string() == p))
        }
        "move_uploaded_file" => {
            let from = arg_str(it, args, 0);
            let to = arg_str(it, args, 1);
            if !it.uploads.iter().any(|u| u.display().to_string() == from) {
                it.warn_pub(&format!(
                    "move_uploaded_file({}): Unable to move: not an uploaded file",
                    from
                ))?;
                Value::Bool(false)
            } else {
                match std::fs::rename(&from, &to).or_else(|_| {
                    std::fs::copy(&from, &to)
                        .map(|_| ())
                        .and_then(|_| std::fs::remove_file(&from))
                }) {
                    Ok(_) => {
                        it.uploads.retain(|u| u.display().to_string() != from);
                        Value::Bool(true)
                    }
                    Err(e) => {
                        it.warn_pub(&format!(
                            "move_uploaded_file(): Unable to move '{}' to '{}': {}",
                            from, to, e
                        ))?;
                        Value::Bool(false)
                    }
                }
            }
        }
        "opendir" | "readdir" | "closedir" | "rewinddir" => Value::Null,
        "fopen" => {
            let path = arg_str(it, args, 0);
            let mode = arg_str(it, args, 1);
            if path == "php://input" {
                let id = it.next_res_id();
                Value::Resource(Rc::new(RefCell::new(PhpResource::Input {
                    id,
                    body: it.php_input.clone(),
                    pos: 0,
                })))
            } else if let Some(body) = parse_data_uri(&path) {
                // `data:[mediatype][;base64],payload` — a memory stream
                // (scalar_* tests fopen a data: URL for test values).
                let id = it.next_res_id();
                Value::Resource(Rc::new(RefCell::new(PhpResource::Input {
                    id,
                    body: Rc::new(body),
                    pos: 0,
                })))
            } else if path == "php://memory"
                || path == "php://temp"
                || path.starts_with("php://temp/maxmemory:")
            {
                // php://memory and php://temp are always read/write.
                let id = it.next_res_id();
                Value::Resource(Rc::new(RefCell::new(PhpResource::Mem {
                    id,
                    buf: Vec::new(),
                    pos: 0,
                    eof: false,
                })))
            } else if let Some(which) = match path.as_str() {
                "php://stdin" => Some(0u8),
                "php://stdout" | "php://output" => Some(1u8),
                "php://stderr" => Some(2u8),
                _ => None,
            } {
                let id = it.next_res_id();
                Value::Resource(Rc::new(RefCell::new(PhpResource::Stdio { id, which })))
            } else {
                match fopen(&path, &mode) {
                    Ok(f) => {
                        let id = it.next_res_id();
                        let (r, w) = mode_flags(&mode);
                        Value::Resource(Rc::new(RefCell::new(PhpResource::File {
                            id,
                            file: f,
                            read: r,
                            write: w,
                            pos: 0,
                            eof: false,
                        })))
                    }
                    Err(e) => {
                        it.warn_pub(&format!("fopen({}): Failed to open stream: {}", path, e))?;
                        Value::Bool(false)
                    }
                }
            }
        }
        "fclose" => {
            if let Some(c) = args.first() {
                *c.borrow_mut() = Value::Null;
            }
            Value::Bool(true)
        }
        "fwrite" | "fputs" => {
            let data = arg(args, 1).to_php_string();
            match write_resource(it, args.first(), data.as_bytes()) {
                Ok(_) => Value::Int(data.len() as i64),
                Err(_) => Value::Bool(false),
            }
        }
        "fread" => {
            let n = arg(args, 1).to_int().max(0) as usize;
            match read_resource(args.first(), n) {
                Ok(b) => Value::str(String::from_utf8_lossy(&b).into_owned()),
                Err(_) => Value::Bool(false),
            }
        }
        "fgets" => match read_line_resource(args.first()) {
            Ok(b) => {
                if b.is_empty() {
                    Value::Bool(false)
                } else {
                    Value::str(String::from_utf8_lossy(&b).into_owned())
                }
            }
            Err(_) => Value::Bool(false),
        },
        "fgetc" => match read_resource(args.first(), 1) {
            Ok(b) if b.is_empty() => Value::Bool(false),
            Ok(b) => Value::str(String::from_utf8_lossy(&b).into_owned()),
            Err(_) => Value::Bool(false),
        },
        "feof" => match args.first() {
            Some(c) => match &*c.borrow() {
                Value::Resource(r) => match &*r.borrow() {
                    PhpResource::File { eof, .. } => Value::Bool(*eof),
                    PhpResource::Mem { eof, .. } => Value::Bool(*eof),
                    _ => Value::Bool(true),
                },
                _ => Value::Bool(true),
            },
            None => Value::Bool(true),
        },
        "fseek" => {
            if let Some(c) = args.first() {
                if let Value::Resource(r) = &*c.borrow() {
                    match &mut *r.borrow_mut() {
                        PhpResource::File { pos, eof, .. } | PhpResource::Mem { pos, eof, .. } => {
                            *pos = arg(args, 1).to_int().max(0) as u64;
                            *eof = false;
                        }
                        _ => {}
                    }
                }
            }
            Value::Int(0)
        }
        "ftell" => match args.first() {
            Some(c) => match &*c.borrow() {
                Value::Resource(r) => match &*r.borrow() {
                    PhpResource::File { pos, .. } | PhpResource::Mem { pos, .. } => {
                        Value::Int(*pos as i64)
                    }
                    _ => Value::Int(0),
                },
                _ => Value::Int(0),
            },
            None => Value::Int(0),
        },
        "rewind" => {
            if let Some(c) = args.first() {
                if let Value::Resource(r) = &*c.borrow() {
                    match &mut *r.borrow_mut() {
                        PhpResource::File { pos, eof, .. } | PhpResource::Mem { pos, eof, .. } => {
                            *pos = 0;
                            *eof = false;
                        }
                        _ => {}
                    }
                }
            }
            Value::Bool(true)
        }
        "ftruncate" => {
            let size = arg(args, 1).to_int().max(0) as usize;
            match args.first() {
                Some(c) => match &*c.borrow() {
                    Value::Resource(r) => match &mut *r.borrow_mut() {
                        PhpResource::Mem { buf, pos, .. } => {
                            buf.resize(size, 0);
                            if (*pos as usize) > size {
                                *pos = size as u64;
                            }
                            Value::Bool(true)
                        }
                        PhpResource::File { file, .. } => {
                            Value::Bool(file.set_len(size as u64).is_ok())
                        }
                        _ => Value::Bool(false),
                    },
                    _ => Value::Bool(false),
                },
                None => Value::Bool(false),
            }
        }
        "fflush" => Value::Bool(true),
        "flock" => Value::Bool(true),
        "fpassthru" => {
            let mut out = Vec::new();
            loop {
                match read_resource(args.first(), 8192) {
                    Ok(b) if b.is_empty() => break,
                    Ok(b) => out.extend_from_slice(&b),
                    Err(_) => break,
                }
            }
            let s = String::from_utf8_lossy(&out);
            it.emit(&s);
            Value::Int(out.len() as i64)
        }
        "fgetcsv" => {
            if args.is_empty() {
                return err(
                    "ArgumentCountError",
                    "fgetcsv() expects at least 1 argument, 0 given",
                );
            }
            // arg1 must be a stream resource.
            if !matches!(&*args[0].borrow(), Value::Resource(_)) {
                return err(
                    "TypeError",
                    format!(
                        "fgetcsv(): Argument #1 ($stream) must be of type resource, {} given",
                        zval_word(&arg(args, 0))
                    ),
                );
            }
            // Omitting $escape is deprecated since PHP 8.4 (emitted per call).
            if args.len() < 5 {
                it.deprecated_pub(
                    "fgetcsv(): the $escape parameter must be provided as its default value will change",
                )?;
            }
            // $length: null or 0 → unlimited; range 0..=i64::MAX-1.
            let length = if args.len() < 2 || matches!(arg(args, 1), Value::Null) {
                0
            } else {
                arg(args, 1).to_int()
            };
            if !(0..=i64::MAX - 1).contains(&length) {
                return err(
                    "ValueError",
                    "fgetcsv(): Argument #2 ($length) must be between 0 and 9223372036854775806",
                );
            }
            let sep = if args.len() < 3 {
                vec![b',']
            } else {
                arg_bs(it, args, 2)
            };
            if sep.len() != 1 {
                return err(
                    "ValueError",
                    "fgetcsv(): Argument #3 ($separator) must be a single character",
                );
            }
            let enc = if args.len() < 4 {
                vec![b'"']
            } else {
                arg_bs(it, args, 3)
            };
            if enc.len() != 1 {
                return err(
                    "ValueError",
                    "fgetcsv(): Argument #4 ($enclosure) must be a single character",
                );
            }
            let esc: Option<u8> = if args.len() < 5 {
                Some(b'\\')
            } else {
                let e = arg_bs(it, args, 4);
                if e.len() > 1 {
                    return err(
                        "ValueError",
                        "fgetcsv(): Argument #5 ($escape) must be empty or a single character",
                    );
                }
                e.first().copied()
            };
            fgetcsv(&args[0], length as usize, sep[0], enc[0], esc)?
        }
        "file" => {
            let path = arg_str(it, args, 0);
            match std::fs::read_to_string(&path) {
                Ok(s) => {
                    let mut a = PhpArray::new();
                    for l in s.split_inclusive('\n') {
                        a.push(Value::str(l.to_string()));
                    }
                    Value::Array(Rc::new(RefCell::new(a)))
                }
                Err(_) => {
                    it.warn_pub(&format!("file({}): Failed to open stream", path))?;
                    Value::Bool(false)
                }
            }
        }
        "readfile" => {
            let path = arg_str(it, args, 0);
            if path == "php://input" {
                let body = it.php_input.clone();
                it.emit(&String::from_utf8_lossy(&body));
                Value::Int(body.len() as i64)
            } else {
                match std::fs::read(&path) {
                    Ok(b) => {
                        let s = String::from_utf8_lossy(&b);
                        it.emit(&s);
                        Value::Int(b.len() as i64)
                    }
                    Err(_) => {
                        it.warn_pub(&format!("readfile({}): Failed to open stream", path))?;
                        Value::Bool(false)
                    }
                }
            }
        }
        "parse_ini_file" | "parse_ini_string" => {
            let s = if name == "parse_ini_file" {
                std::fs::read_to_string(fs_path(&arg_str(it, args, 0))).unwrap_or_default()
            } else {
                arg_str(it, args, 0)
            };
            let mut out = PhpArray::new();
            let mut section: Option<String> = None;
            for line in s.lines() {
                let l = line.trim();
                if l.is_empty() || l.starts_with(';') || l.starts_with('#') {
                    continue;
                }
                if l.starts_with('[') && l.ends_with(']') {
                    section = Some(l[1..l.len() - 1].to_string());
                    continue;
                }
                if let Some((k, v)) = l.split_once('=') {
                    let k = k.trim().to_string();
                    let v = v.trim().trim_matches('"').to_string();
                    let _key = match &section {
                        Some(s) => format!("{}.{}", s, k),
                        None => k.clone(),
                    };
                    out.set(to_key(&Value::str(k)), Value::str(v));
                }
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "stat" => match std::fs::metadata(fs_path(&arg_str(it, args, 0))) {
            Ok(m) => Value::Array(Rc::new(RefCell::new(stat_array(&m)))),
            Err(_) => Value::Bool(false),
        },
        "lstat" => match std::fs::symlink_metadata(fs_path(&arg_str(it, args, 0))) {
            Ok(m) => Value::Array(Rc::new(RefCell::new(stat_array(&m)))),
            Err(_) => Value::Bool(false),
        },
        "clearstatcache" => Value::Null,
        "umask" => Value::Int(0o022),
        "chmod" | "chown" | "chgrp" | "touch" => Value::Bool(true),
        "link" | "symlink" | "readlink" | "linkinfo" => Value::Bool(false),
        "disk_free_space" | "disk_total_space" => Value::Float(1e12),
        "fnmatch" => Value::Bool(false),
        "stream_get_contents" => {
            // (resource, ?length = null, offset = -1): an explicit
            // offset seeks first — UnifiedDiffOutputBuilder writes a
            // php://memory buffer then reads it back from 0.
            if let Some(Value::Resource(r)) = args.first().map(|c| c.borrow().clone()) {
                let offset = args.get(2).map(|c| c.borrow().to_int()).unwrap_or(-1);
                if offset >= 0 {
                    match &mut *r.borrow_mut() {
                        PhpResource::File { pos, eof, .. } | PhpResource::Mem { pos, eof, .. } => {
                            *pos = offset as u64;
                            *eof = false;
                        }
                        PhpResource::Input { pos, .. } => {
                            *pos = offset as u64;
                        }
                        _ => {}
                    }
                }
            }
            let maxlen = match args.get(1).map(|c| c.borrow().clone()) {
                Some(Value::Null) | None => -1,
                Some(v) => v.to_int(),
            };
            let mut remaining = if maxlen < 0 {
                usize::MAX
            } else {
                maxlen as usize
            };
            let mut out = Vec::new();
            while remaining > 0 {
                match read_resource(args.first(), remaining.min(8192)) {
                    Ok(b) if b.is_empty() => break,
                    Ok(b) => {
                        remaining = remaining.saturating_sub(b.len());
                        out.extend_from_slice(&b);
                    }
                    Err(_) => break,
                }
            }
            Value::str(String::from_utf8_lossy(&out).into_owned())
        }
        "stream_copy_to_stream" => {
            let maxlen = args.get(2).map(|c| c.borrow().to_int()).unwrap_or(-1);
            let offset = args.get(3).map(|c| c.borrow().to_int()).unwrap_or(0);
            if offset > 0 {
                if let Some(Value::Resource(r)) = args.first().map(|c| c.borrow().clone()) {
                    match &mut *r.borrow_mut() {
                        PhpResource::File { pos, eof, .. } | PhpResource::Mem { pos, eof, .. } => {
                            *pos = offset as u64;
                            *eof = false;
                        }
                        PhpResource::Input { pos, .. } => {
                            *pos = offset as u64;
                        }
                        _ => {}
                    }
                }
            }
            let mut total: i64 = 0;
            let mut remaining = if maxlen < 0 { i64::MAX } else { maxlen };
            let mut ok = true;
            while remaining > 0 {
                let want = remaining.min(8192) as usize;
                match read_resource(args.first(), want) {
                    Ok(b) if b.is_empty() => break,
                    Ok(b) => {
                        total += b.len() as i64;
                        remaining -= b.len() as i64;
                        if write_resource(it, args.get(1), &b).is_err() {
                            ok = false;
                            break;
                        }
                    }
                    Err(_) => {
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                Value::Int(total)
            } else {
                Value::Bool(false)
            }
        }
        "stream_context_create" | "stream_context_get_default" => {
            Value::Resource(Rc::new(RefCell::new(PhpResource::Other {
                id: it.next_res_id(),
                kind: "stream-context",
            })))
        }
        "stream_context_set_option" | "stream_context_get_options" => Value::Bool(true),
        "stream_wrapper_register" | "stream_wrapper_unregister" => Value::Bool(false),
        "stream_isatty" | "posix_isatty" => Value::Bool(false),
        "stream_set_timeout"
        | "stream_set_blocking"
        | "stream_set_read_buffer"
        | "stream_set_write_buffer"
        | "stream_set_chunk_size" => Value::Bool(true),
        "stream_get_meta_data" | "stream_get_filters" | "stream_get_wrappers" => {
            Value::Array(Rc::new(RefCell::new(PhpArray::new())))
        }
        "stream_filter_register" | "stream_filter_append" | "stream_filter_prepend" => {
            Value::Bool(false)
        }
        "fstat" => match arg(args, 0) {
            Value::Resource(r) => {
                let meta = {
                    let res = r.borrow();
                    match &*res {
                        crate::value::PhpResource::File { file, .. } => file.metadata().ok(),
                        crate::value::PhpResource::Stdio { which, .. } => {
                            std::fs::metadata(match which {
                                0 => "/dev/stdin",
                                1 => "/dev/stdout",
                                _ => "/dev/stderr",
                            })
                            .ok()
                        }
                        _ => None,
                    }
                };
                match meta {
                    Some(m) => Value::Array(Rc::new(RefCell::new(stat_array(&m)))),
                    None => Value::Bool(false),
                }
            }
            _ => Value::Bool(false),
        },
        "fdopen" | "popen" | "pclose" => Value::Bool(false),
        _ => return Ok(None),
    }))
}

// ----- helpers -----

fn read_stream(path: &str) -> Result<Vec<u8>, std::io::Error> {
    if path.starts_with("php://stdin") {
        return Ok(Vec::new());
    }
    std::fs::read(fs_path(path))
}

/// Strips the `file://` stream wrapper — PHP treats `file:///abs/path`
/// (and `file://localhost/...`) as a plain local path.
fn fs_path(p: &str) -> &str {
    match p.strip_prefix("file://") {
        Some(rest) => rest.strip_prefix("localhost").unwrap_or(rest),
        None => p,
    }
}

/// `data:[mediatype][;base64],payload` wrapper — returns the decoded
/// payload bytes, or None when the path isn't a data: URI.
/// `data:` and `data://` forms both work (scalar_* tests).
fn parse_data_uri(path: &str) -> Option<Vec<u8>> {
    let rest = path
        .strip_prefix("data:")
        .or_else(|| path.strip_prefix("data://"))?;
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let comma = rest.find(',')?;
    let (meta, payload) = (&rest[..comma], &rest[comma + 1..]);
    if meta.split(';').any(|m| m.eq_ignore_ascii_case("base64")) {
        base64_decode(payload)
    } else {
        Some(percent_decode(payload.as_bytes()))
    }
}

/// URL percent-decoding for data: URIs (`%41` -> 'A', '+' stays literal).
fn percent_decode(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = (b[i + 1] as char).to_digit(16);
            let l = (b[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (h, l) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn mode_flags(mode: &str) -> (bool, bool) {
    let m = mode.chars().next().unwrap_or('r');
    let plus = mode.contains('+');
    match m {
        'r' => (true, plus),
        'w' | 'a' | 'x' | 'c' => (plus, true),
        _ => (true, true),
    }
}

fn fopen(path: &str, mode: &str) -> std::io::Result<std::fs::File> {
    let path = fs_path(path);
    use std::fs::OpenOptions;
    let m = mode.chars().next().unwrap_or('r');
    let plus = mode.contains('+');
    let mut o = OpenOptions::new();
    match m {
        'r' => {
            o.read(true);
            if plus {
                o.write(true);
            }
        }
        'w' => {
            o.write(true).create(true).truncate(true);
            if plus {
                o.read(true);
            }
        }
        'a' => {
            o.append(true).create(true);
            if plus {
                o.read(true);
            }
        }
        'x' => {
            o.write(true).create_new(true);
            if plus {
                o.read(true);
            }
        }
        'c' => {
            o.write(true).create(true);
            if plus {
                o.read(true);
            }
        }
        _ => {
            o.read(true);
        }
    }
    o.open(path)
}

pub(in crate::builtins) fn write_resource(
    it: &mut Interp,
    c: Option<&Cell>,
    data: &[u8],
) -> Result<(), PhpError> {
    use std::io::{Seek, Write};
    match c.map(|c| c.borrow().clone()) {
        Some(Value::Resource(r)) => {
            let mut rb = r.borrow_mut();
            match &mut *rb {
                PhpResource::Stdio { which, .. } => match *which {
                    1 => {
                        if it.live_io {
                            let mut so = std::io::stdout().lock();
                            let _ = so.write_all(data);
                            let _ = so.flush();
                        } else {
                            it.out.extend_from_slice(data);
                        }
                        Ok(())
                    }
                    2 => {
                        if it.live_io {
                            let _ = std::io::stderr().write_all(data);
                        } else {
                            it.err_buf.push_str(&String::from_utf8_lossy(data));
                        }
                        Ok(())
                    }
                    _ => Err(PhpError::fatal("not writable", 0)),
                },
                PhpResource::File {
                    file, pos, write, ..
                } => {
                    if !*write {
                        return Err(PhpError::fatal("not writable", 0));
                    }
                    let _ = file.seek(std::io::SeekFrom::Start(*pos));
                    file.write_all(data)
                        .map_err(|e| PhpError::fatal(e.to_string(), 0))?;
                    *pos += data.len() as u64;
                    Ok(())
                }
                PhpResource::Mem { buf, pos, eof, .. } => {
                    let start = *pos as usize;
                    if start > buf.len() {
                        buf.resize(start, 0);
                    }
                    let end = (start + data.len()).min(buf.len());
                    if start < end {
                        buf[start..end].copy_from_slice(&data[..end - start]);
                    }
                    buf.extend_from_slice(&data[end - start..]);
                    *pos += data.len() as u64;
                    *eof = false;
                    Ok(())
                }
                _ => Err(PhpError::fatal("bad resource", 0)),
            }
        }
        _ => Err(PhpError::fatal("not a resource", 0)),
    }
}

fn read_resource(c: Option<&Cell>, n: usize) -> Result<Vec<u8>, PhpError> {
    use std::io::{Read, Seek};
    match c.map(|c| c.borrow().clone()) {
        Some(Value::Resource(r)) => {
            let mut rb = r.borrow_mut();
            match &mut *rb {
                PhpResource::Stdio { .. } => Ok(Vec::new()),
                PhpResource::Input { body, pos, .. } => {
                    let avail = body.len().saturating_sub(*pos as usize);
                    let take = avail.min(n);
                    let out = body[*pos as usize..*pos as usize + take].to_vec();
                    *pos += take as u64;
                    Ok(out)
                }
                PhpResource::Mem { buf, pos, eof, .. } => {
                    let avail = buf.len().saturating_sub(*pos as usize);
                    let take = avail.min(n);
                    let out = buf[*pos as usize..*pos as usize + take].to_vec();
                    *pos += take as u64;
                    if take < n {
                        *eof = true;
                    }
                    Ok(out)
                }
                PhpResource::File {
                    file,
                    pos,
                    read,
                    eof,
                    ..
                } => {
                    if !*read || *eof {
                        return Ok(Vec::new());
                    }
                    let _ = file.seek(std::io::SeekFrom::Start(*pos));
                    let mut buf = vec![0u8; n];
                    match file.read(&mut buf) {
                        Ok(got) => {
                            buf.truncate(got);
                            *pos += got as u64;
                            if got < n {
                                *eof = true;
                            }
                            Ok(buf)
                        }
                        Err(e) => Err(PhpError::fatal(e.to_string(), 0)),
                    }
                }
                _ => Ok(Vec::new()),
            }
        }
        _ => Err(PhpError::fatal("not a resource", 0)),
    }
}

fn read_line_resource(c: Option<&Cell>) -> Result<Vec<u8>, PhpError> {
    use std::io::{Read, Seek};
    match c.map(|c| c.borrow().clone()) {
        Some(Value::Resource(r)) => {
            let mut rb = r.borrow_mut();
            match &mut *rb {
                PhpResource::Stdio { .. } => Ok(Vec::new()),
                PhpResource::Input { body, pos, .. } => {
                    let start = *pos as usize;
                    if start >= body.len() {
                        Ok(Vec::new())
                    } else {
                        let nl = body[start..]
                            .iter()
                            .position(|b| *b == b'\n')
                            .map(|o| start + o + 1)
                            .unwrap_or(body.len());
                        let out = body[start..nl].to_vec();
                        *pos = nl as u64;
                        Ok(out)
                    }
                }
                PhpResource::Mem { buf, pos, eof, .. } => {
                    let start = *pos as usize;
                    if start >= buf.len() {
                        *eof = true;
                        Ok(Vec::new())
                    } else {
                        let nl = buf[start..]
                            .iter()
                            .position(|b| *b == b'\n')
                            .map(|o| start + o + 1)
                            .unwrap_or(buf.len());
                        let out = buf[start..nl].to_vec();
                        *pos = nl as u64;
                        Ok(out)
                    }
                }
                PhpResource::File {
                    file,
                    pos,
                    read,
                    eof,
                    ..
                } => {
                    if !*read || *eof {
                        return Ok(Vec::new());
                    }
                    let _ = file.seek(std::io::SeekFrom::Start(*pos));
                    let mut out = Vec::new();
                    let mut byte = [0u8; 1];
                    loop {
                        match file.read(&mut byte) {
                            Ok(0) => {
                                *eof = true;
                                break;
                            }
                            Ok(_) => {
                                out.push(byte[0]);
                                *pos += 1;
                                if byte[0] == b'\n' {
                                    break;
                                }
                            }
                            Err(e) => return Err(PhpError::fatal(e.to_string(), 0)),
                        }
                    }
                    Ok(out)
                }
                _ => Ok(Vec::new()),
            }
        }
        _ => Err(PhpError::fatal("not a resource", 0)),
    }
}

/// PHP zval type word used in TypeError "…, X given" messages.
fn zval_word(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::Int(_) => "int".into(),
        Value::Float(_) => "float".into(),
        Value::Str(_) => "string".into(),
        Value::Array(_) => "array".into(),
        Value::Object(o) => o.borrow().class.name().to_string(),
        Value::Callable(_) => "Closure".into(),
        Value::Resource(_) => "resource".into(),
    }
}

/// php_stream_gets: read up to `limit` bytes, stopping after '\n'.
/// Returns an empty vec at EOF (or on a non-readable stream).
fn csv_gets(c: &Cell, limit: usize) -> Result<Vec<u8>, PhpError> {
    use std::io::{Read, Seek};
    match c.borrow().clone() {
        Value::Resource(r) => {
            let mut rb = r.borrow_mut();
            match &mut *rb {
                PhpResource::File {
                    file,
                    pos,
                    read,
                    eof,
                    ..
                } => {
                    if !*read || *eof {
                        return Ok(Vec::new());
                    }
                    let _ = file.seek(std::io::SeekFrom::Start(*pos));
                    let mut out = Vec::new();
                    let mut byte = [0u8; 1];
                    while out.len() < limit {
                        match file.read(&mut byte) {
                            Ok(0) => {
                                *eof = true;
                                break;
                            }
                            Ok(_) => {
                                out.push(byte[0]);
                                *pos += 1;
                                if byte[0] == b'\n' {
                                    break;
                                }
                            }
                            Err(e) => return Err(PhpError::fatal(e.to_string(), 0)),
                        }
                    }
                    Ok(out)
                }
                PhpResource::Mem { buf, pos, eof, .. } => {
                    let start = *pos as usize;
                    if start >= buf.len() {
                        *eof = true;
                        Ok(Vec::new())
                    } else {
                        let mut end = start;
                        while end < buf.len() && end - start < limit {
                            let b = buf[end];
                            end += 1;
                            if b == b'\n' {
                                break;
                            }
                        }
                        *pos = end as u64;
                        Ok(buf[start..end].to_vec())
                    }
                }
                PhpResource::Input { body, pos, .. } => {
                    let start = *pos as usize;
                    if start >= body.len() {
                        Ok(Vec::new())
                    } else {
                        let mut end = start;
                        while end < body.len() && end - start < limit {
                            let b = body[end];
                            end += 1;
                            if b == b'\n' {
                                break;
                            }
                        }
                        *pos = end as u64;
                        Ok(body[start..end].to_vec())
                    }
                }
                _ => Ok(Vec::new()),
            }
        }
        _ => Ok(Vec::new()),
    }
}

/// php_stream_get_line equivalent: read the rest of the current line
/// (through '\n' inclusive), unbounded. Returns None at EOF.
fn csv_get_line(c: &Cell) -> Result<Option<Vec<u8>>, PhpError> {
    let out = csv_gets(c, usize::MAX)?;
    Ok(if out.is_empty() { None } else { Some(out) })
}

/// C isspace() for the byte domain (space, \t, \n, \v, \f, \r).
fn c_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// Index of the first byte of the trailing-whitespace run (Zend's
/// php_fgetcsv_lookup_trailing_spaces). buf[limit..] is the "line end"
/// bytes that get embedded when an enclosure spans buffers.
fn trailing_spaces_limit(buf: &[u8]) -> usize {
    let mut i = buf.len();
    while i > 0 && c_space(buf[i - 1]) {
        i -= 1;
    }
    i
}

fn rtrim_len(field: &[u8]) -> usize {
    let mut i = field.len();
    while i > 0 && c_space(field[i - 1]) {
        i -= 1;
    }
    i
}

/// fgetcsv: read one CSV record from a stream. Faithful port of Zend's
/// php_fgetcsv (ext/standard/file.c): the first chunk is a `length`-bounded
/// gets (0 = whole line); enclosed fields that stay open at buffer end pull
/// in whole further lines, embedding each buffer's trailing whitespace.
/// Escape only acts inside enclosures and is kept literally.
fn fgetcsv(
    stream: &Cell,
    length: usize,
    sep: u8,
    enc: u8,
    esc: Option<u8>,
) -> Result<Value, PhpError> {
    let limit = if length == 0 { usize::MAX } else { length };
    let mut buf = csv_gets(stream, limit)?;
    if buf.is_empty() {
        return Ok(Value::Bool(false));
    }
    let mut limit_i = trailing_spaces_limit(&buf);
    let mut fields: Vec<Vec<u8>> = Vec::new();
    let mut bptr = 0usize;
    let mut first_field = true;
    let mut blank = false;

    loop {
        let inc = bptr < limit_i;
        if inc {
            // Skip a leading whitespace run when it leads to an enclosure
            // (the whitespace is then dropped from the field).
            let mut tmp = bptr;
            while tmp < buf.len() && buf[tmp] != sep && c_space(buf[tmp]) {
                tmp += 1;
            }
            if tmp < limit_i && buf[tmp] == enc {
                bptr = tmp;
            }
        }
        if first_field && bptr == limit_i {
            // Whole buffer was trailing whitespace → NULL row → [null].
            blank = true;
            break;
        }
        first_field = false;

        let mut tptr: Vec<u8> = Vec::new();
        if inc && buf[bptr] == enc {
            // Enclosure-delimited field.
            bptr += 1;
            let mut hunk = bptr;
            // state: 0 normal, 1 just saw escape, 2 just saw enclosure
            let mut st = 0u8;
            'enc: loop {
                if bptr >= limit_i {
                    match st {
                        2 => {
                            // Buffer ended right after the closing quote.
                            tptr.extend_from_slice(&buf[hunk..bptr - 1]);
                            hunk = bptr;
                            break 'enc;
                        }
                        _ => {
                            tptr.extend_from_slice(&buf[hunk..bptr]);
                            hunk = bptr;
                            // Embed this buffer's trailing whitespace.
                            tptr.extend_from_slice(&buf[limit_i..]);
                            match csv_get_line(stream)? {
                                None => break 'enc,
                                Some(nb) => {
                                    buf = nb;
                                    bptr = 0;
                                    hunk = 0;
                                    limit_i = trailing_spaces_limit(&buf);
                                    st = 0;
                                }
                            }
                        }
                    }
                } else {
                    let c = buf[bptr];
                    match st {
                        1 => {
                            // Escaped char: consumed literally (escape kept).
                            bptr += 1;
                            st = 0;
                        }
                        2 => {
                            if c != enc {
                                // Real closing quote.
                                tptr.extend_from_slice(&buf[hunk..bptr - 1]);
                                hunk = bptr;
                                break 'enc;
                            }
                            // `""` pair → one literal quote.
                            tptr.extend_from_slice(&buf[hunk..bptr]);
                            bptr += 1;
                            hunk = bptr;
                            st = 0;
                        }
                        _ => {
                            if c == enc {
                                st = 2;
                            } else if esc == Some(c) {
                                st = 1;
                            }
                            bptr += 1;
                        }
                    }
                }
            }
            // Post-enclosure: append junk up to the next delimiter.
            while bptr < limit_i && buf[bptr] != sep {
                bptr += 1;
            }
            tptr.extend_from_slice(&buf[hunk..bptr]);
            if bptr < limit_i && buf[bptr] == sep {
                bptr += 1;
                fields.push(std::mem::take(&mut tptr));
            } else {
                fields.push(std::mem::take(&mut tptr));
                break;
            }
        } else {
            // Non-enclosure field: scan to delimiter/buffer end, rtrim.
            let fstart = bptr;
            while bptr < limit_i && buf[bptr] != sep {
                bptr += 1;
            }
            let fend = fstart + rtrim_len(&buf[fstart..bptr]);
            fields.push(buf[fstart..fend].to_vec());
            if bptr < limit_i && buf[bptr] == sep {
                bptr += 1;
            } else {
                break;
            }
        }
    }

    let mut a = PhpArray::new();
    if blank {
        a.push(Value::Null);
    } else {
        for f in fields {
            a.push(Value::bytes(f));
        }
    }
    Ok(Value::Array(Rc::new(RefCell::new(a))))
}

fn glob_to_regex(pat: &str) -> regex::Regex {
    let mut r = String::from("^");
    for c in pat.chars() {
        match c {
            '*' => r.push_str(".*"),
            '?' => r.push('.'),
            c => r.push_str(&regex::escape(&c.to_string())),
        }
    }
    r.push('$');
    regex::Regex::new(&r).unwrap_or_else(|_| regex::Regex::new("a^").unwrap())
}

/// stat()/fstat() shape: numeric keys 0..=12 followed by the named keys —
/// dev ino mode nlink uid gid rdev size atime mtime ctime blksize blocks.
#[cfg(unix)]
fn stat_array(m: &std::fs::Metadata) -> PhpArray {
    use std::os::unix::fs::MetadataExt;
    let vals = [
        m.dev() as i64,
        m.ino() as i64,
        m.mode() as i64,
        m.nlink() as i64,
        m.uid() as i64,
        m.gid() as i64,
        m.rdev() as i64,
        m.size() as i64,
        m.atime(),
        m.mtime(),
        m.ctime(),
        m.blksize() as i64,
        m.blocks() as i64,
    ];
    let mut a = PhpArray::new();
    for (i, v) in vals.iter().enumerate() {
        a.set(ArrKey::Int(i as i64), Value::Int(*v));
    }
    for (name, v) in [
        "dev", "ino", "mode", "nlink", "uid", "gid", "rdev", "size", "atime", "mtime", "ctime",
        "blksize", "blocks",
    ]
    .iter()
    .zip(vals.iter())
    {
        a.set(to_key(&Value::str(*name)), Value::Int(*v));
    }
    a
}
