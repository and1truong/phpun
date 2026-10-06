//! Filesystem/stream builtins: file fns, stream resources, stat, glob.

use super::crypto::base64_decode;
use super::string::{zpp_long, zpp_long_arg};
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
                        path: name.display().to_string(),
                        mode: "w+b".into(),
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
                // php://memory and php://temp are always read/write
                // internally; fwrite still honors the fopen mode.
                let id = it.next_res_id();
                let w = mem_writeable(&mode);
                Value::Resource(Rc::new(RefCell::new(PhpResource::Mem {
                    id,
                    buf: Vec::new(),
                    pos: 0,
                    eof: false,
                    write: w,
                })))
            } else if let Some(which) = match path.as_str() {
                "php://stdin" => Some(0u8),
                "php://stdout" => Some(1u8),
                "php://stderr" => Some(2u8),
                // php://output writes through the output-buffer chain
                // (ob_* can capture them), unlike php://stdout.
                "php://output" => Some(3u8),
                _ => None,
            } {
                let id = it.next_res_id();
                Value::Resource(Rc::new(RefCell::new(PhpResource::Stdio { id, which })))
            } else {
                match fopen_mode(&mode) {
                    None => {
                        it.warn_pub(&format!(
                            "fopen({}): Failed to open stream: `{}' is not a valid mode for fopen",
                            path, mode
                        ))?;
                        Value::Bool(false)
                    }
                    Some(spec) => match open_file(&path, &spec) {
                        Ok(f) => {
                            let id = it.next_res_id();
                            Value::Resource(Rc::new(RefCell::new(PhpResource::File {
                                id,
                                file: f,
                                read: spec.read,
                                write: spec.write,
                                pos: 0,
                                eof: false,
                                path: path.clone(),
                                mode: mode.clone(),
                            })))
                        }
                        Err(e) => {
                            it.warn_pub(&format!(
                                "fopen({}): Failed to open stream: {}",
                                path,
                                io_errno_str(&e).1
                            ))?;
                            Value::Bool(false)
                        }
                    },
                }
            }
        }
        "fclose" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            // zend_list_close marks the resource itself closed — every
            // alias ($f2 = $f) sees `resource (closed)`, not just $f.
            if let Some(c) = args.first() {
                if let Value::Resource(r) = &*c.borrow() {
                    let mut rb = r.borrow_mut();
                    let id = rb.id();
                    *rb = PhpResource::Closed { id };
                }
            }
            Value::Bool(true)
        }
        "fwrite" | "fputs" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            let data = arg(args, 1).to_php_string();
            match write_resource(it, args.first(), data.as_bytes())? {
                StreamWrite::Written => Value::Int(data.len() as i64),
                StreamWrite::Partial(n) => Value::Int(n as i64),
                StreamWrite::Ebadf(errno, msg) => {
                    write_ebadf_notice(it, name, data.len(), errno, &msg)?;
                    Value::Bool(false)
                }
                StreamWrite::Discarded => Value::Bool(false),
            }
        }
        "fread" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            let n = arg(args, 1).to_int().max(0) as usize;
            match read_resource(args.first(), n)? {
                StreamRead::Data(b) => Value::str(String::from_utf8_lossy(&b).into_owned()),
                StreamRead::Ebadf(errno, msg) => {
                    read_ebadf_notice(it, name, errno, &msg)?;
                    Value::Bool(false)
                }
            }
        }
        "fgets" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            // ?int $length = null: zend reads at most $length-1 bytes
            // (php_stream_gets size-1), newline included in the budget.
            let limit = if args.len() < 2 || matches!(arg(args, 1), Value::Null) {
                usize::MAX
            } else {
                let n = zpp_long(it, args, 1, name, 2, "$length", "?int")?;
                if n <= 0 {
                    return err(
                        "ValueError",
                        "fgets(): Argument #2 ($length) must be greater than 0",
                    );
                }
                (n - 1) as usize
            };
            match read_line_resource(args.first(), limit)? {
                StreamRead::Data(b) => {
                    if b.is_empty() {
                        Value::Bool(false)
                    } else {
                        Value::str(String::from_utf8_lossy(&b).into_owned())
                    }
                }
                StreamRead::Ebadf(errno, msg) => {
                    read_ebadf_notice(it, name, errno, &msg)?;
                    Value::Bool(false)
                }
            }
        }
        "fgetc" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            match read_resource(args.first(), 1)? {
                StreamRead::Data(b) if b.is_empty() => Value::Bool(false),
                StreamRead::Data(b) => Value::str(String::from_utf8_lossy(&b).into_owned()),
                StreamRead::Ebadf(errno, msg) => {
                    read_ebadf_notice(it, name, errno, &msg)?;
                    Value::Bool(false)
                }
            }
        }
        "feof" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            match args.first() {
                Some(c) => match &*c.borrow() {
                    Value::Resource(r) => match &*r.borrow() {
                        PhpResource::File { eof, .. } => Value::Bool(*eof),
                        PhpResource::Mem { eof, .. } => Value::Bool(*eof),
                        PhpResource::Pipe { eof, .. } => Value::Bool(*eof),
                        _ => Value::Bool(true),
                    },
                    _ => Value::Bool(true),
                },
                None => Value::Bool(true),
            }
        }
        "fseek" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            // zend zpp: (resource, int offset, int whence = SEEK_SET).
            let offset = zpp_long_arg(it, args, 1, name, 2, "$offset")?;
            let whence = zpp_long_arg(it, args, 2, name, 3, "$whence")?;
            if let Some(c) = args.first() {
                if let Value::Resource(r) = &*c.borrow() {
                    match &mut *r.borrow_mut() {
                        PhpResource::File { file, pos, eof, .. } => {
                            use std::io::Seek;
                            let len = file.metadata().map(|m| m.len()).unwrap_or(0) as i64;
                            let new_pos = match whence {
                                0 => Some(offset),
                                1 => Some(*pos as i64 + offset),
                                2 => Some(len + offset),
                                _ => None,
                            };
                            match new_pos {
                                // zend: lseek EINVAL → silent -1,
                                // position unchanged.
                                Some(np) if np >= 0 => {
                                    *pos = np as u64;
                                    *eof = false;
                                    let _ = file.seek(std::io::SeekFrom::Start(*pos));
                                }
                                _ => return Ok(Some(Value::Int(-1))),
                            }
                        }
                        PhpResource::Mem { buf, pos, eof, .. } => {
                            let new_pos = match whence {
                                0 => Some(offset),
                                1 => Some(*pos as i64 + offset),
                                2 => Some(buf.len() as i64 + offset),
                                _ => None,
                            };
                            match new_pos {
                                Some(np) if np >= 0 => {
                                    *pos = np as u64;
                                    *eof = false;
                                }
                                _ => return Ok(Some(Value::Int(-1))),
                            }
                        }
                        PhpResource::Pipe { .. } => {
                            it.warn_pub(&format!("{}(): Stream does not support seeking", name))?;
                            return Ok(Some(Value::Int(-1)));
                        }
                        _ => {}
                    }
                }
            }
            Value::Int(0)
        }
        "ftell" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            match args.first() {
                Some(c) => match &*c.borrow() {
                    Value::Resource(r) => match &*r.borrow() {
                        PhpResource::File { pos, .. } | PhpResource::Mem { pos, .. } => {
                            Value::Int(*pos as i64)
                        }
                        // zend's pipe position lags one byte behind the
                        // consumed count: false before any read, then
                        // bytes_consumed-1.
                        PhpResource::Pipe { pos, .. } => {
                            if *pos == 0 {
                                Value::Bool(false)
                            } else {
                                Value::Int(*pos as i64 - 1)
                            }
                        }
                        _ => Value::Int(0),
                    },
                    _ => Value::Int(0),
                },
                None => Value::Int(0),
            }
        }
        "rewind" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            if let Some(c) = args.first() {
                if let Value::Resource(r) = &*c.borrow() {
                    match &mut *r.borrow_mut() {
                        PhpResource::File { file, pos, eof, .. } => {
                            use std::io::Seek;
                            *pos = 0;
                            *eof = false;
                            let _ = file.seek(std::io::SeekFrom::Start(0));
                        }
                        PhpResource::Mem { pos, eof, .. } => {
                            *pos = 0;
                            *eof = false;
                        }
                        PhpResource::Pipe { .. } => {
                            it.warn_pub(&format!("{}(): Stream does not support seeking", name))?;
                            return Ok(Some(Value::Bool(false)));
                        }
                        _ => {}
                    }
                }
            }
            Value::Bool(true)
        }
        "ftruncate" => {
            stream_open_check(args, 0, name, 1, "stream")?;
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
        "fflush" | "flock" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            Value::Bool(true)
        }
        "fpassthru" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            let mut out = Vec::new();
            let mut failed = false;
            loop {
                match read_resource(args.first(), 8192)? {
                    StreamRead::Data(b) if b.is_empty() => break,
                    StreamRead::Data(b) => out.extend_from_slice(&b),
                    StreamRead::Ebadf(errno, msg) => {
                        read_ebadf_notice(it, name, errno, &msg)?;
                        failed = true;
                        break;
                    }
                }
            }
            let s = String::from_utf8_lossy(&out);
            it.emit(&s);
            // zend returns -1 when the passthrough read failed.
            Value::Int(if failed { -1 } else { out.len() as i64 })
        }
        "fgetcsv" => {
            if args.is_empty() {
                return err(
                    "ArgumentCountError",
                    "fgetcsv() expects at least 1 argument, 0 given",
                );
            }
            // arg1 must be an open stream resource.
            stream_open_check(args, 0, name, 1, "stream")?;
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
            fgetcsv(it, name, &args[0], length as usize, sep[0], enc[0], esc)?
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
            stream_open_check(args, 0, name, 1, "stream")?;
            // (resource, ?length = null, offset = -1): an explicit
            // offset seeks first — UnifiedDiffOutputBuilder writes a
            // php://memory buffer then reads it back from 0.
            if let Some(Value::Resource(r)) = args.first().map(|c| c.borrow().clone()) {
                let offset = args.get(2).map(|c| c.borrow().to_int()).unwrap_or(-1);
                if offset >= 0 {
                    match &mut *r.borrow_mut() {
                        PhpResource::File { file, pos, eof, .. } => {
                            use std::io::Seek;
                            *pos = offset as u64;
                            *eof = false;
                            let _ = file.seek(std::io::SeekFrom::Start(*pos));
                        }
                        PhpResource::Mem { pos, eof, .. } => {
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
                match read_resource(args.first(), remaining.min(8192))? {
                    StreamRead::Data(b) if b.is_empty() => break,
                    StreamRead::Data(b) => {
                        remaining = remaining.saturating_sub(b.len());
                        out.extend_from_slice(&b);
                    }
                    StreamRead::Ebadf(errno, msg) => {
                        read_ebadf_notice(it, name, errno, &msg)?;
                        break;
                    }
                }
            }
            Value::str(String::from_utf8_lossy(&out).into_owned())
        }
        "stream_copy_to_stream" => {
            stream_open_check(args, 0, name, 1, "from")?;
            stream_open_check(args, 1, name, 2, "to")?;
            let maxlen = args.get(2).map(|c| c.borrow().to_int()).unwrap_or(-1);
            let offset = args.get(3).map(|c| c.borrow().to_int()).unwrap_or(0);
            if offset > 0 {
                if let Some(Value::Resource(r)) = args.first().map(|c| c.borrow().clone()) {
                    match &mut *r.borrow_mut() {
                        PhpResource::File { file, pos, eof, .. } => {
                            use std::io::Seek;
                            *pos = offset as u64;
                            *eof = false;
                            let _ = file.seek(std::io::SeekFrom::Start(*pos));
                        }
                        PhpResource::Mem { pos, eof, .. } => {
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
                match read_resource(args.first(), want)? {
                    StreamRead::Data(b) if b.is_empty() => break,
                    StreamRead::Data(b) => {
                        total += b.len() as i64;
                        remaining -= b.len() as i64;
                        match write_resource(it, args.get(1), &b)? {
                            StreamWrite::Written | StreamWrite::Partial(_) => {}
                            StreamWrite::Ebadf(errno, msg) => {
                                write_ebadf_notice(it, name, b.len(), errno, &msg)?;
                                ok = false;
                                break;
                            }
                            StreamWrite::Discarded => {
                                ok = false;
                                break;
                            }
                        }
                    }
                    StreamRead::Ebadf(errno, msg) => {
                        read_ebadf_notice(it, name, errno, &msg)?;
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
        "stream_isatty" | "posix_isatty" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            Value::Bool(false)
        }
        "stream_set_blocking" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            let mode = arg(args, 1).is_truthy();
            let Value::Resource(r) = arg(args, 0) else {
                return Ok(Some(Value::Bool(false)));
            };
            let mut rb = r.borrow_mut();
            match &mut *rb {
                PhpResource::Pipe { file, nonblock, .. } => {
                    use std::os::fd::AsRawFd;
                    *nonblock = !mode;
                    let fd = file.as_raw_fd();
                    set_fd_nonblock(fd, mode);
                    Value::Bool(true)
                }
                PhpResource::File { file, .. } => {
                    use std::os::fd::AsRawFd;
                    set_fd_nonblock(file.as_raw_fd(), mode);
                    Value::Bool(true)
                }
                _ => Value::Bool(true),
            }
        }
        "stream_select" => return stream_select(it, name, args),
        "stream_set_timeout"
        | "stream_set_read_buffer"
        | "stream_set_write_buffer"
        | "stream_set_chunk_size" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            Value::Bool(true)
        }
        "stream_get_meta_data" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            let mk = |pairs: Vec<(&'static str, Value)>| {
                let mut a = PhpArray::new();
                for (k, v) in pairs {
                    a.set(ArrKey::Str(k.into()), v);
                }
                Value::Array(Rc::new(RefCell::new(a)))
            };
            let mut base = vec![
                ("timed_out", Value::Bool(false)),
                ("blocked", Value::Bool(true)),
            ];
            match arg(args, 0) {
                Value::Resource(r) => {
                    let rb = r.borrow();
                    match &*rb {
                        PhpResource::Pipe {
                            write,
                            socket,
                            eof,
                            nonblock,
                            ..
                        } => {
                            base[1] = ("blocked", Value::Bool(!*nonblock));
                            base.push(("eof", Value::Bool(*eof)));
                            base.push((
                                "stream_type",
                                Value::str(if *socket { "generic_socket" } else { "STDIO" }),
                            ));
                            base.push((
                                "mode",
                                Value::str(if *socket {
                                    "r+"
                                } else if *write {
                                    "w"
                                } else {
                                    "r"
                                }),
                            ));
                            base.push(("unread_bytes", Value::Int(0)));
                            base.push(("seekable", Value::Bool(false)));
                            mk(base)
                        }
                        PhpResource::File {
                            file,
                            eof,
                            path,
                            mode,
                            ..
                        } => {
                            use std::os::fd::AsRawFd;
                            base[1] = ("blocked", Value::Bool(!fd_is_nonblock(file.as_raw_fd())));
                            base.push(("eof", Value::Bool(*eof)));
                            base.push(("wrapper_type", Value::str("plainfile")));
                            base.push(("stream_type", Value::str("STDIO")));
                            base.push(("mode", Value::str(mode.clone())));
                            base.push(("unread_bytes", Value::Int(0)));
                            base.push(("seekable", Value::Bool(true)));
                            base.push(("uri", Value::str(path.clone())));
                            mk(base)
                        }
                        PhpResource::Stdio { which, .. } => {
                            base.push(("eof", Value::Bool(false)));
                            base.push(("wrapper_type", Value::str("PHP")));
                            base.push(("stream_type", Value::str("STDIO")));
                            base.push(("mode", Value::str(if *which == 0 { "rb" } else { "wb" })));
                            base.push(("unread_bytes", Value::Int(0)));
                            // zend probes the fd: lseek(fd,0,SEEK_CUR)
                            // fails on pipes/ttys, succeeds on files.
                            let seekable = unsafe {
                                libc::lseek(*which as libc::c_int, 0, libc::SEEK_CUR) != -1
                            };
                            base.push(("seekable", Value::Bool(seekable)));
                            base.push((
                                "uri",
                                Value::str(match which {
                                    0 => "php://stdin",
                                    1 => "php://stdout",
                                    _ => "php://stderr",
                                }),
                            ));
                            mk(base)
                        }
                        PhpResource::Mem { eof, .. } => {
                            base.push(("eof", Value::Bool(*eof)));
                            base.push(("wrapper_type", Value::str("PHP")));
                            base.push(("stream_type", Value::str("TEMP")));
                            base.push(("mode", Value::str("w+b")));
                            base.push(("unread_bytes", Value::Int(0)));
                            base.push(("seekable", Value::Bool(true)));
                            mk(base)
                        }
                        PhpResource::Input { body, pos, .. } => {
                            base.push(("eof", Value::Bool(*pos as usize >= body.len())));
                            base.push(("wrapper_type", Value::str("PHP")));
                            base.push(("stream_type", Value::str("Input")));
                            base.push(("mode", Value::str("rb")));
                            base.push(("unread_bytes", Value::Int(0)));
                            base.push(("seekable", Value::Bool(false)));
                            mk(base)
                        }
                        _ => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
                    }
                }
                _ => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
            }
        }
        "stream_get_filters" | "stream_get_wrappers" => {
            Value::Array(Rc::new(RefCell::new(PhpArray::new())))
        }
        "stream_filter_register" | "stream_filter_append" | "stream_filter_prepend" => {
            Value::Bool(false)
        }
        "fstat" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            match arg(args, 0) {
                Value::Resource(r) => {
                    let meta = {
                        let res = r.borrow();
                        match &*res {
                            crate::value::PhpResource::File { file, .. } => file.metadata().ok(),
                            crate::value::PhpResource::Pipe { file, .. } => file.metadata().ok(),
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
            }
        }
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

/// php_stream_parse_fopen_modes: `mode[0]` must be r/w/a/x/c
/// (anything else — 'br', 'tr', '', ... — is an invalid-mode warning
/// + false) and '+' anywhere in the string adds O_RDWR.
struct FopenSpec {
    read: bool,
    write: bool,
    append: bool,
    create: bool,
    truncate: bool,
    exclusive: bool,
}

fn fopen_mode(mode: &str) -> Option<FopenSpec> {
    let plus = mode.contains('+');
    Some(match mode.as_bytes().first().copied()? {
        b'r' => FopenSpec {
            read: true,
            write: plus,
            append: false,
            create: false,
            truncate: false,
            exclusive: false,
        },
        b'w' => FopenSpec {
            read: plus,
            write: true,
            append: false,
            create: true,
            truncate: true,
            exclusive: false,
        },
        b'a' => FopenSpec {
            read: plus,
            write: true,
            append: true,
            create: true,
            truncate: false,
            exclusive: false,
        },
        b'x' => FopenSpec {
            read: plus,
            write: true,
            append: false,
            create: true,
            truncate: false,
            exclusive: true,
        },
        b'c' => FopenSpec {
            read: plus,
            write: true,
            append: false,
            create: true,
            truncate: false,
            exclusive: false,
        },
        _ => return None,
    })
}

/// php_stream_mode_from_str (strpbrk) — the mode parser for
/// php://memory|temp only: 'w', 'a' or '+' anywhere → writeable.
fn mem_writeable(mode: &str) -> bool {
    mode.bytes().any(|b| matches!(b, b'w' | b'a' | b'+'))
}

/// fcntl helper for stream_set_blocking: set/clear O_NONBLOCK on a
/// real fd (zend applies the flag to plain files as well as pipes).
fn set_fd_nonblock(fd: std::os::fd::RawFd, blocking: bool) {
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl >= 0 {
            let fl2 = if blocking {
                fl & !libc::O_NONBLOCK
            } else {
                fl | libc::O_NONBLOCK
            };
            libc::fcntl(fd, libc::F_SETFL, fl2);
        }
    }
}

fn fd_is_nonblock(fd: std::os::fd::RawFd) -> bool {
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        fl >= 0 && fl & libc::O_NONBLOCK != 0
    }
}

/// `PHP_Z_PARAM_STREAM` shared by every stream builtin: the argument
/// must be a resource holding an *open stream* — a plain zval gets the
/// "must be of type resource, T given" TypeError while a closed handle
/// or a non-stream resource (stream-context) gets "must be an open
/// stream resource" (zend `php_stream_from_zval` failure).
pub(in crate::builtins) fn stream_open_check(
    args: &[Cell],
    i: usize,
    fname: &str,
    pnum: usize,
    pname: &str,
) -> Result<(), PhpError> {
    match arg(args, i) {
        Value::Resource(r) => match &*r.borrow() {
            // A proc_open() handle is a resource but not a stream —
            // zend's stream fns reject it the same as a closed one.
            PhpResource::Closed { .. } | PhpResource::Proc { .. } | PhpResource::Other { .. } => {
                err(
                    "TypeError",
                    format!(
                        "{}(): Argument #{} (${}) must be an open stream resource",
                        fname, pnum, pname
                    ),
                )
            }
            _ => Ok(()),
        },
        v => err(
            "TypeError",
            format!(
                "{}(): Argument #{} (${}) must be of type resource, {} given",
                fname,
                pnum,
                pname,
                zval_word(&v)
            ),
        ),
    }
}

/// Outcome of a `php_stream_write` attempt. The plain wrapper reports
/// failures as `Ebadf` (caller raises the "Write of N bytes failed with
/// errno=E STR" E_NOTICE); read-only php://memory|temp and php://input
/// discard silently (no write op → -1/0 with no diagnostic).
pub(in crate::builtins) enum StreamWrite {
    Written,
    /// Short write on a pipe (or a 0-byte EAGAIN on a nonblocking
    /// one): php_stream_write returns the count, fwrite reports it.
    Partial(usize),
    Ebadf(i32, String),
    Discarded,
}

/// Outcome of a `php_stream_read` attempt — same E_NOTICE contract on
/// a descriptor not open for reading.
pub(in crate::builtins) enum StreamRead {
    Data(Vec<u8>),
    Ebadf(i32, String),
}

/// errno + strerror() pair for a plain-wrapper IO failure; Rust's
/// `io::Error` Display is "STR (os error N)" — strip the suffix.
fn io_errno_str(e: &std::io::Error) -> (i32, String) {
    let n = e.raw_os_error().unwrap_or(9);
    let msg = e.to_string();
    let suffix = format!(" (os error {})", n);
    (n, msg.strip_suffix(&suffix).unwrap_or(&msg).to_string())
}

/// proc_open()'s `['file', path, mode]` descriptor — the same mode
/// table fopen uses (proc_open validates the mode letters itself).
pub(in crate::builtins) fn fopen(path: &str, mode: &str) -> std::io::Result<std::fs::File> {
    match fopen_mode(mode) {
        Some(spec) => open_file(path, &spec),
        None => open_file(path, &fopen_mode("r").unwrap()),
    }
}

/// `<fn>(): Write of N bytes failed with errno=E STR` E_NOTICE (the
/// plain stdio wrapper's diagnostic in `php_stream_stdio_write`).
pub(in crate::builtins) fn write_ebadf_notice(
    it: &mut Interp,
    fname: &str,
    len: usize,
    errno: i32,
    msg: &str,
) -> Result<(), PhpError> {
    it.notice_pub(&format!(
        "{}(): Write of {} bytes failed with errno={} {}",
        fname, len, errno, msg
    ))
}

/// `<fn>(): Read of 8192 bytes failed with errno=E STR` E_NOTICE —
/// reads go through 8192-byte stream chunks regardless of the
/// requested length.
pub(in crate::builtins) fn read_ebadf_notice(
    it: &mut Interp,
    fname: &str,
    errno: i32,
    msg: &str,
) -> Result<(), PhpError> {
    it.notice_pub(&format!(
        "{}(): Read of 8192 bytes failed with errno={} {}",
        fname, errno, msg
    ))
}

fn open_file(path: &str, spec: &FopenSpec) -> std::io::Result<std::fs::File> {
    let path = fs_path(path);
    use std::fs::OpenOptions;
    let mut o = OpenOptions::new();
    o.read(spec.read).write(spec.write);
    if spec.create {
        if spec.exclusive {
            o.create_new(true);
        } else {
            o.create(true);
        }
    }
    o.truncate(spec.truncate).append(spec.append);
    o.open(path)
}

pub(in crate::builtins) fn write_resource(
    it: &mut Interp,
    c: Option<&Cell>,
    data: &[u8],
) -> Result<StreamWrite, PhpError> {
    use std::io::Write;
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
                        Ok(StreamWrite::Written)
                    }
                    3 => {
                        it.emit_bytes(data);
                        Ok(StreamWrite::Written)
                    }
                    2 => {
                        if it.live_io {
                            let _ = std::io::stderr().write_all(data);
                        } else {
                            it.err_buf.push_str(&String::from_utf8_lossy(data));
                        }
                        Ok(StreamWrite::Written)
                    }
                    // STDIN: the fd exists but isn't open for writing —
                    // write(2) returns EBADF.
                    _ => Ok(StreamWrite::Ebadf(9, "Bad file descriptor".into())),
                },
                PhpResource::Pipe {
                    file,
                    write,
                    pos,
                    nonblock,
                    ..
                } => {
                    if !*write {
                        return Ok(StreamWrite::Ebadf(9, "Bad file descriptor".into()));
                    }
                    match file.write(data) {
                        Ok(n) => {
                            *pos += n as u64;
                            if n == data.len() {
                                Ok(StreamWrite::Written)
                            } else {
                                Ok(StreamWrite::Partial(n))
                            }
                        }
                        Err(e) if *nonblock && e.kind() == std::io::ErrorKind::WouldBlock => {
                            Ok(StreamWrite::Partial(0))
                        }
                        Err(e) => {
                            let (errno, msg) = io_errno_str(&e);
                            Ok(StreamWrite::Ebadf(errno, msg))
                        }
                    }
                }
                PhpResource::File {
                    file, pos, write, ..
                } => {
                    if !*write {
                        return Ok(StreamWrite::Ebadf(9, "Bad file descriptor".into()));
                    }
                    // Writes land at the real fd offset (shared with
                    // dup'd child ends), like zend's fwrite(3).
                    if let Err(e) = file.write_all(data) {
                        let (errno, msg) = io_errno_str(&e);
                        return Ok(StreamWrite::Ebadf(errno, msg));
                    }
                    *pos += data.len() as u64;
                    Ok(StreamWrite::Written)
                }
                PhpResource::Mem {
                    buf,
                    pos,
                    eof,
                    write,
                    ..
                } => {
                    // TEMP_STREAM_READONLY → php_stream_memory_write
                    // returns -1 and the bytes are silently dropped
                    // (no E_NOTICE — only plain stdio notices).
                    if !*write {
                        return Ok(StreamWrite::Discarded);
                    }
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
                    Ok(StreamWrite::Written)
                }
                PhpResource::Input { .. } => Ok(StreamWrite::Discarded),
                _ => Err(PhpError::fatal("bad resource", 0)),
            }
        }
        _ => Err(PhpError::fatal("not a resource", 0)),
    }
}

/// Read up to `n` bytes from a pipe fd. EOF (read 0) latches `eof`;
/// EAGAIN on a nonblocking stream reads "" silently — zend returns
/// "" there rather than retrying.
fn read_pipe(
    file: &mut std::fs::File,
    eof: &mut bool,
    nonblock: bool,
    pos: &mut u64,
    n: usize,
) -> Result<StreamRead, PhpError> {
    use std::io::Read;
    if *eof {
        return Ok(StreamRead::Data(Vec::new()));
    }
    let mut buf = vec![0u8; n];
    match file.read(&mut buf) {
        Ok(0) => {
            *eof = true;
            Ok(StreamRead::Data(Vec::new()))
        }
        Ok(got) => {
            buf.truncate(got);
            *pos += got as u64;
            Ok(StreamRead::Data(buf))
        }
        Err(e) if nonblock && e.kind() == std::io::ErrorKind::WouldBlock => {
            Ok(StreamRead::Data(Vec::new()))
        }
        Err(e) => {
            let (errno, msg) = io_errno_str(&e);
            Ok(StreamRead::Ebadf(errno, msg))
        }
    }
}

/// php_stream_gets on a pipe: byte-wise reads up to `limit` bytes,
/// stopping after '\n'.
fn read_line_pipe(
    file: &mut std::fs::File,
    eof: &mut bool,
    nonblock: bool,
    pos: &mut u64,
    limit: usize,
) -> Result<StreamRead, PhpError> {
    use std::io::Read;
    if *eof {
        return Ok(StreamRead::Data(Vec::new()));
    }
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
            Err(e) if nonblock && e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => {
                let (errno, msg) = io_errno_str(&e);
                return Ok(StreamRead::Ebadf(errno, msg));
            }
        }
    }
    Ok(StreamRead::Data(out))
}

fn read_resource(c: Option<&Cell>, n: usize) -> Result<StreamRead, PhpError> {
    use std::io::Read;
    match c.map(|c| c.borrow().clone()) {
        Some(Value::Resource(r)) => {
            let mut rb = r.borrow_mut();
            match &mut *rb {
                PhpResource::Pipe {
                    file,
                    pos,
                    eof,
                    nonblock,
                    ..
                } => read_pipe(file, eof, *nonblock, pos, n),
                PhpResource::Stdio { which, .. } => match *which {
                    // STDIN reads are not modeled; STDOUT/STDERR/
                    // php://output are write-only fds → read(2) EBADF.
                    0 => Ok(StreamRead::Data(Vec::new())),
                    _ => Ok(StreamRead::Ebadf(9, "Bad file descriptor".into())),
                },
                PhpResource::Input { body, pos, .. } => {
                    // pos may sit past the end (fseek allows it) —
                    // clamp the slice start instead of panicking.
                    let start = (*pos as usize).min(body.len());
                    let take = (body.len() - start).min(n);
                    let out = body[start..start + take].to_vec();
                    *pos += take as u64;
                    Ok(StreamRead::Data(out))
                }
                PhpResource::Mem { buf, pos, eof, .. } => {
                    let start = (*pos as usize).min(buf.len());
                    let take = (buf.len() - start).min(n);
                    let out = buf[start..start + take].to_vec();
                    *pos += take as u64;
                    if take < n {
                        *eof = true;
                    }
                    Ok(StreamRead::Data(out))
                }
                PhpResource::File {
                    file,
                    pos,
                    read,
                    eof,
                    ..
                } => {
                    if !*read {
                        return Ok(StreamRead::Ebadf(9, "Bad file descriptor".into()));
                    }
                    if *eof {
                        return Ok(StreamRead::Data(Vec::new()));
                    }
                    // zend reads at the real fd offset — an fd dup'd to
                    // a proc_open child shares it, so a child draining
                    // the descriptor moves our reads to EOF too. `pos`
                    // is the PHP-side ftell counter (+= bytes read).
                    let mut buf = vec![0u8; n];
                    match file.read(&mut buf) {
                        Ok(got) => {
                            buf.truncate(got);
                            *pos += got as u64;
                            if got < n {
                                *eof = true;
                            }
                            Ok(StreamRead::Data(buf))
                        }
                        Err(e) => {
                            let (errno, msg) = io_errno_str(&e);
                            Ok(StreamRead::Ebadf(errno, msg))
                        }
                    }
                }
                _ => Ok(StreamRead::Data(Vec::new())),
            }
        }
        _ => Err(PhpError::fatal("not a resource", 0)),
    }
}

fn read_line_resource(c: Option<&Cell>, limit: usize) -> Result<StreamRead, PhpError> {
    use std::io::Read;
    match c.map(|c| c.borrow().clone()) {
        Some(Value::Resource(r)) => {
            let mut rb = r.borrow_mut();
            match &mut *rb {
                PhpResource::Pipe {
                    file,
                    pos,
                    eof,
                    nonblock,
                    ..
                } => read_line_pipe(file, eof, *nonblock, pos, limit),
                PhpResource::Stdio { which, .. } => match *which {
                    0 => Ok(StreamRead::Data(Vec::new())),
                    _ => Ok(StreamRead::Ebadf(9, "Bad file descriptor".into())),
                },
                PhpResource::Input { body, pos, .. } => {
                    let start = *pos as usize;
                    if start >= body.len() {
                        Ok(StreamRead::Data(Vec::new()))
                    } else {
                        let nl = body[start..]
                            .iter()
                            .position(|b| *b == b'\n')
                            .map(|o| start + o + 1)
                            .unwrap_or(body.len())
                            .min(start.saturating_add(limit));
                        let out = body[start..nl].to_vec();
                        *pos = nl as u64;
                        Ok(StreamRead::Data(out))
                    }
                }
                PhpResource::Mem { buf, pos, eof, .. } => {
                    let start = *pos as usize;
                    if start >= buf.len() {
                        *eof = true;
                        Ok(StreamRead::Data(Vec::new()))
                    } else {
                        let nl = buf[start..]
                            .iter()
                            .position(|b| *b == b'\n')
                            .map(|o| start + o + 1)
                            .unwrap_or(buf.len())
                            .min(start.saturating_add(limit));
                        let out = buf[start..nl].to_vec();
                        *pos = nl as u64;
                        Ok(StreamRead::Data(out))
                    }
                }
                PhpResource::File {
                    file,
                    pos,
                    read,
                    eof,
                    ..
                } => {
                    if !*read {
                        return Ok(StreamRead::Ebadf(9, "Bad file descriptor".into()));
                    }
                    if *eof {
                        return Ok(StreamRead::Data(Vec::new()));
                    }
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
                            Err(e) => {
                                let (errno, msg) = io_errno_str(&e);
                                return Ok(StreamRead::Ebadf(errno, msg));
                            }
                        }
                    }
                    Ok(StreamRead::Data(out))
                }
                _ => Ok(StreamRead::Data(Vec::new())),
            }
        }
        _ => Err(PhpError::fatal("not a resource", 0)),
    }
}

/// php_stream_gets: read up to `limit` bytes, stopping after '\n'.
/// Returns an empty vec at EOF (or on a non-readable stream).
fn csv_gets(c: &Cell, limit: usize) -> Result<StreamRead, PhpError> {
    use std::io::Read;
    match c.borrow().clone() {
        Value::Resource(r) => {
            let mut rb = r.borrow_mut();
            match &mut *rb {
                PhpResource::Pipe {
                    file,
                    pos,
                    eof,
                    nonblock,
                    ..
                } => read_line_pipe(file, eof, *nonblock, pos, limit),
                PhpResource::File {
                    file,
                    pos,
                    read,
                    eof,
                    ..
                } => {
                    if !*read {
                        return Ok(StreamRead::Ebadf(9, "Bad file descriptor".into()));
                    }
                    if *eof {
                        return Ok(StreamRead::Data(Vec::new()));
                    }
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
                            Err(e) => {
                                let (errno, msg) = io_errno_str(&e);
                                return Ok(StreamRead::Ebadf(errno, msg));
                            }
                        }
                    }
                    Ok(StreamRead::Data(out))
                }
                PhpResource::Mem { buf, pos, eof, .. } => {
                    let start = *pos as usize;
                    if start >= buf.len() {
                        *eof = true;
                        Ok(StreamRead::Data(Vec::new()))
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
                        Ok(StreamRead::Data(buf[start..end].to_vec()))
                    }
                }
                PhpResource::Input { body, pos, .. } => {
                    let start = *pos as usize;
                    if start >= body.len() {
                        Ok(StreamRead::Data(Vec::new()))
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
                        Ok(StreamRead::Data(body[start..end].to_vec()))
                    }
                }
                _ => Ok(StreamRead::Data(Vec::new())),
            }
        }
        _ => Ok(StreamRead::Data(Vec::new())),
    }
}

/// php_stream_get_line equivalent: read the rest of the current line
/// (through '\n' inclusive), unbounded. Returns None at EOF.
fn csv_get_line(c: &Cell) -> Result<StreamRead, PhpError> {
    match csv_gets(c, usize::MAX)? {
        StreamRead::Data(out) if out.is_empty() => Ok(StreamRead::Data(Vec::new())),
        other => Ok(other),
    }
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
    it: &mut Interp,
    name: &str,
    stream: &Cell,
    length: usize,
    sep: u8,
    enc: u8,
    esc: Option<u8>,
) -> Result<Value, PhpError> {
    let limit = if length == 0 { usize::MAX } else { length };
    let mut buf = match csv_gets(stream, limit)? {
        StreamRead::Data(b) => b,
        StreamRead::Ebadf(errno, msg) => {
            read_ebadf_notice(it, name, errno, &msg)?;
            return Ok(Value::Bool(false));
        }
    };
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
                                StreamRead::Data(nb) if nb.is_empty() => break 'enc,
                                StreamRead::Data(nb) => {
                                    buf = nb;
                                    bptr = 0;
                                    hunk = 0;
                                    limit_i = trailing_spaces_limit(&buf);
                                    st = 0;
                                }
                                StreamRead::Ebadf(errno, msg) => {
                                    read_ebadf_notice(it, name, errno, &msg)?;
                                    return Ok(Value::Bool(false));
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

/// stream_select(&$read, &$write, &$except, ?$seconds, ?$usec): libc
/// select() over the fd of each resource in the input arrays, then the
/// arrays are rewritten to only the ready entries. NULL $seconds waits
/// forever (zend's NULL timeval).
fn stream_select(it: &mut Interp, fname: &str, args: &[Cell]) -> Result<Option<Value>, PhpError> {
    use std::os::fd::AsRawFd;
    let mut sets: [Vec<(ArrKey, Value, i32)>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    for (ai, arg_c) in args.iter().take(3).enumerate() {
        let v = arg_c.borrow().clone();
        match v {
            Value::Array(a) => {
                for (k, c) in a.borrow().iter() {
                    let item = c.borrow().clone();
                    match item {
                        Value::Resource(ref r) => {
                            let fd = {
                                let rb = r.borrow();
                                match &*rb {
                                    PhpResource::File { file, .. }
                                    | PhpResource::Pipe { file, .. } => file.as_raw_fd(),
                                    PhpResource::Stdio { which, .. } => *which as i32,
                                    other => {
                                        let ty = other.type_name();
                                        it.warn_pub(&format!(
                                            "{}(): cannot represent a stream of type {} as a File Descriptor",
                                            fname, ty
                                        ))?;
                                        continue;
                                    }
                                }
                            };
                            sets[ai].push((k.clone(), item, fd));
                        }
                        other => {
                            it.warn_pub(&format!(
                                "{}(): cannot represent a stream of type {} as a File Descriptor",
                                fname,
                                other.gettype()
                            ))?;
                        }
                    }
                }
            }
            Value::Null => {}
            _ => {
                return err(
                    "TypeError",
                    format!(
                        "{}(): Argument #{} must be of type ?array, {} given",
                        fname,
                        ai + 1,
                        v.gettype()
                    ),
                )
            }
        }
    }
    if sets.iter().all(|s| s.is_empty()) {
        return err("ValueError", "No stream arrays were passed");
    }
    let mut fds = [
        unsafe { std::mem::zeroed::<libc::fd_set>() },
        unsafe { std::mem::zeroed::<libc::fd_set>() },
        unsafe { std::mem::zeroed::<libc::fd_set>() },
    ];
    let mut nfds = 0i32;
    for (i, s) in sets.iter().enumerate() {
        for (_, _, fd) in s {
            unsafe {
                libc::FD_SET(*fd, &mut fds[i]);
            }
            nfds = nfds.max(*fd + 1);
        }
    }
    let has_tv = args
        .get(3)
        .map(|c| !matches!(*c.borrow(), Value::Null))
        .unwrap_or(false);
    let mut tv = libc::timeval {
        tv_sec: args.get(3).map(|c| c.borrow().to_int()).unwrap_or(0),
        tv_usec: args.get(4).map(|c| c.borrow().to_int()).unwrap_or(0),
    };
    let n = unsafe {
        libc::select(
            nfds,
            &mut fds[0],
            &mut fds[1],
            &mut fds[2],
            if has_tv {
                &mut tv
            } else {
                std::ptr::null_mut()
            },
        )
    };
    if n < 0 {
        let e = std::io::Error::last_os_error();
        let (en, reason) = io_errno_str(&e);
        it.warn_pub(&format!(
            "stream_select(): unable to select [{}]: {} (max_fd={})",
            en, reason, nfds
        ))?;
        return Ok(Some(Value::Bool(false)));
    }
    // Rewrite each input array: keep only ready entries.
    for (i, s) in sets.iter().enumerate() {
        let mut ready = PhpArray::new();
        for (k, v, fd) in s {
            if unsafe { libc::FD_ISSET(*fd, &fds[i]) } {
                ready.set(k.clone(), v.clone());
            }
        }
        if let Some(c) = args.get(i) {
            *c.borrow_mut() = Value::Array(Rc::new(RefCell::new(ready)));
        }
    }
    Ok(Some(Value::Int(n as i64)))
}
