//! Filesystem/stream builtins: file fns, stream resources, stat, glob.

use super::crypto::base64_decode;
use super::string::{zpp_long, zpp_long_arg};
use super::*;
use crate::interp::CallArgs;
use crate::value::{CodecKind, Dechunk, DechunkState, FilterState, StreamFilter};

/// Live compressor/decompressor objects for FilterState::Codec
/// entries — keyed by filter id in Interp::codec_states (flate2 and
/// bzip2 state objects don't implement Clone).
pub enum CodecState {
    /// `flushed`/`finished` mirror zend's bookkeeping: deflate arms
    /// track whether the last data pass flushed (init true — nothing
    /// pending); inflate arms latch Z_STREAM_END.
    ZlibDeflate {
        strm: flate2::Compress,
        flushed: bool,
    },
    ZlibInflate {
        strm: flate2::Decompress,
        finished: bool,
    },
    BzDeflate {
        strm: bzip2::Compress,
        flushed: bool,
    },
    BzInflate {
        strm: bzip2::Decompress,
        finished: bool,
    },
}

impl CodecState {
    fn new(kind: CodecKind) -> Self {
        match kind {
            // zend's zlib stream filter: default level in RAW RFC1951
            // mode (windowBits -15 — no zlib/gzip header).
            CodecKind::ZlibDeflate => CodecState::ZlibDeflate {
                strm: flate2::Compress::new(Default::default(), false),
                flushed: true,
            },
            CodecKind::ZlibInflate => CodecState::ZlibInflate {
                strm: flate2::Decompress::new(false),
                finished: false,
            },
            // zend's bz2 filter: block size 4 ('BZh4' on the wire),
            // default work factor.
            CodecKind::BzDeflate => CodecState::BzDeflate {
                strm: bzip2::Compress::new(bzip2::Compression::new(4), 30),
                flushed: true,
            },
            CodecKind::BzInflate => CodecState::BzInflate {
                strm: bzip2::Decompress::new(false),
                finished: false,
            },
        }
    }
}

/// Codec decode failures that zend reports with a NOTICE + a fatal
/// stream read (the fill dies, fread returns false).
enum CodecErr {
    /// '{ctx}(): zlib: data error'
    Zlib,
    /// '{ctx}(): bzip2 decompression failed'
    Bz,
}

/// The PSFS flag set a filter() pass runs under. zend's write path
/// sends PSFS_FLAG_NORMAL on fwrite, PSFS_FLAG_FLUSH_INC on
/// fflush/seek, PSFS_FLAG_FLUSH_CLOSE at close; the read path sends
/// NORMAL/FLUSH_CLOSE.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CodecMode {
    Normal,
    Inc,
    Close,
}

/// One filter() pass over a streaming codec — mirrors zend's
/// php_zlib_*_filter / php_bz2_*_filter: the input is fed under the
/// call's flush mode, then a tail loop drains pending output on
/// FLUSH_CLOSE (always) or FLUSH_INC (only when the data pass didn't
/// already flush). *_vec APIs write into spare capacity only, and
/// total_in/out are cumulative across calls.
fn codec_run(c: &mut CodecState, input: &[u8], mode: CodecMode) -> Result<Vec<u8>, CodecErr> {
    let mut out = Vec::new();
    match c {
        CodecState::ZlibDeflate { strm: z, flushed } => {
            use flate2::{FlushCompress, Status};
            let feed = match mode {
                CodecMode::Close => FlushCompress::Full,
                CodecMode::Inc => FlushCompress::Sync,
                CodecMode::Normal => FlushCompress::None,
            };
            let base_in = z.total_in();
            loop {
                let done = (z.total_in() - base_in) as usize;
                if done >= input.len() {
                    break;
                }
                let (tin, tout) = (z.total_in(), z.total_out());
                out.reserve(64 * 1024);
                match z.compress_vec(&input[done..], &mut out, feed) {
                    Ok(_) => {
                        *flushed = feed != FlushCompress::None;
                        if z.total_in() == tin && z.total_out() == tout {
                            break;
                        }
                    }
                    Err(_) => return Err(CodecErr::Zlib),
                }
            }
            if mode == CodecMode::Close || (mode == CodecMode::Inc && !*flushed) {
                let tail = if mode == CodecMode::Close {
                    FlushCompress::Finish
                } else {
                    FlushCompress::Sync
                };
                // miniz emits a fresh 00-00-ff-ff marker on EVERY
                // Sync call (unlike zlib's one-per-flush + BUF_ERROR),
                // so drain only while output fills the buffer — a
                // short write means the codec is empty.
                loop {
                    out.reserve(64 * 1024);
                    let cap = out.capacity() - out.len();
                    let before = out.len();
                    match z.compress_vec(&[], &mut out, tail) {
                        Ok(Status::Ok) if out.len() - before >= cap => {}
                        Ok(_) => break,
                        Err(_) => return Err(CodecErr::Zlib),
                    }
                }
                *flushed = true;
            }
            Ok(out)
        }
        CodecState::ZlibInflate { strm: z, finished } => {
            use flate2::{FlushDecompress, Status};
            let feed = if mode == CodecMode::Close {
                FlushDecompress::Finish
            } else {
                FlushDecompress::Sync
            };
            let base_in = z.total_in();
            let mut err = false;
            loop {
                let done = (z.total_in() - base_in) as usize;
                if done >= input.len() || *finished {
                    break;
                }
                let (tin, tout) = (z.total_in(), z.total_out());
                out.reserve(64 * 1024);
                match z.decompress_vec(&input[done..], &mut out, feed) {
                    Ok(Status::StreamEnd) => {
                        *finished = true;
                        break;
                    }
                    Ok(_) => {
                        if z.total_in() == tin && z.total_out() == tout {
                            break;
                        }
                    }
                    Err(_) => {
                        err = true;
                        break;
                    }
                }
            }
            if !*finished && mode == CodecMode::Close {
                // zend's closing tail: drain with Z_FINISH.
                loop {
                    let (tin, tout) = (z.total_in(), z.total_out());
                    out.reserve(64 * 1024);
                    match z.decompress_vec(&[], &mut out, FlushDecompress::Finish) {
                        Ok(Status::Ok) => {}
                        Ok(Status::StreamEnd) => {
                            *finished = true;
                            break;
                        }
                        Ok(_) => break,
                        Err(_) => {
                            err = true;
                            break;
                        }
                    }
                    if z.total_in() == tin && z.total_out() == tout {
                        break;
                    }
                }
            }
            if err {
                return Err(CodecErr::Zlib);
            }
            Ok(out)
        }
        CodecState::BzDeflate { strm: b, flushed } => {
            use bzip2::{Action, Status};
            let feed = match mode {
                CodecMode::Close => Action::Finish,
                CodecMode::Inc => Action::Flush,
                CodecMode::Normal => Action::Run,
            };
            let base_in = b.total_in();
            loop {
                let done = (b.total_in() - base_in) as usize;
                if done >= input.len() {
                    break;
                }
                let (tin, tout) = (b.total_in(), b.total_out());
                out.reserve(64 * 1024);
                match b.compress_vec(&input[done..], &mut out, feed) {
                    Ok(Status::StreamEnd) => {
                        *flushed = true;
                        break;
                    }
                    Ok(_) => {
                        *flushed = feed != Action::Run;
                        if b.total_in() == tin && b.total_out() == tout {
                            break;
                        }
                    }
                    Err(_) => return Err(CodecErr::Bz),
                }
            }
            if mode == CodecMode::Close || (mode == CodecMode::Inc && !*flushed) {
                let tail = if mode == CodecMode::Close {
                    Action::Finish
                } else {
                    Action::Flush
                };
                // zend's do/while: keep draining while the codec says
                // *OK (FINISH_OK → finishes at STREAM_END).
                let keep = if tail == Action::Finish {
                    Status::FinishOk
                } else {
                    Status::FlushOk
                };
                loop {
                    out.reserve(64 * 1024);
                    match b.compress_vec(&[], &mut out, tail) {
                        Ok(s) if s == keep => {}
                        Ok(_) => break,
                        Err(_) => return Err(CodecErr::Bz),
                    }
                }
                *flushed = true;
            }
            Ok(out)
        }
        CodecState::BzInflate { strm: b, finished } => {
            use bzip2::Status;
            let base_in = b.total_in();
            let mut err = false;
            loop {
                let done = (b.total_in() - base_in) as usize;
                if done >= input.len() || *finished {
                    break;
                }
                let (tin, tout) = (b.total_in(), b.total_out());
                out.reserve(64 * 1024);
                match b.decompress_vec(&input[done..], &mut out) {
                    Ok(Status::StreamEnd) => {
                        *finished = true;
                        break;
                    }
                    Ok(_) => {
                        if b.total_in() == tin && b.total_out() == tout {
                            break;
                        }
                    }
                    Err(_) => {
                        err = true;
                        break;
                    }
                }
            }
            if !*finished && mode == CodecMode::Close {
                loop {
                    let (tin, tout) = (b.total_in(), b.total_out());
                    out.reserve(64 * 1024);
                    match b.decompress_vec(&[], &mut out) {
                        Ok(Status::Ok) => {}
                        Ok(Status::StreamEnd) => {
                            *finished = true;
                            break;
                        }
                        Ok(_) => break,
                        Err(_) => {
                            err = true;
                            break;
                        }
                    }
                    if b.total_in() == tin && b.total_out() == tout {
                        break;
                    }
                }
            }
            if err {
                return Err(CodecErr::Bz);
            }
            Ok(out)
        }
    }
}

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
    // zend's php_error_docref prefixes warnings with the active builtin.
    it.filter_warn_ctx.clear();
    it.filter_warn_ctx.push_str(name);
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
            } else if let Some((segs, inner)) = php_filter_uri(&path) {
                let Some(inner) = inner else {
                    return Err(PhpError::uncaught("Error", "No URL resource specified", 0));
                };
                match open_filter_resource(it, &segs, &inner, "rb", "file_get_contents", &path)? {
                    Some(Value::Resource(r)) => {
                        let c = cell(Value::Resource(r));
                        let mut out = Vec::new();
                        loop {
                            match read_resource(it, Some(&c), 8192)? {
                                StreamRead::Data(b) if !b.is_empty() => out.extend(b),
                                _ => break,
                            }
                        }
                        Value::str(String::from_utf8_lossy(&out).into_owned())
                    }
                    _ => Value::Bool(false),
                }
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
            // zend's php_stream_fopen_tmpfile → php_open_temporary_fd:
            // a real, unique, LINKED file — mode r+b — that the stream
            // removes on close (PhpResource::Drop honors
            // unlink_on_close).
            match php_open_temporary_fd() {
                Some((fd, name)) => {
                    use std::os::unix::io::FromRawFd;
                    let f = unsafe { std::fs::File::from_raw_fd(fd) };
                    let id = it.next_res_id();
                    Value::Resource(Rc::new(RefCell::new(PhpResource::File {
                        id,
                        file: f,
                        read: true,
                        write: true,
                        pos: 0,
                        eof: false,
                        rbuf: Default::default(),
                        rcap: 0,
                        unlink_on_close: true,
                        path: name,
                        mode: "r+b".into(),
                    })))
                }
                None => Value::Bool(false),
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
            if let Some((segs, inner)) = php_filter_uri(&path) {
                let Some(inner) = inner else {
                    return Err(PhpError::uncaught("Error", "No URL resource specified", 0));
                };
                match open_filter_resource(it, &segs, &inner, &mode, "fopen", &path)? {
                    Some(v) => return Ok(Some(v)),
                    None => return Ok(Some(Value::Bool(false))),
                }
            }
            if path == "php://input" {
                let id = it.next_res_id();
                Value::Resource(Rc::new(RefCell::new(PhpResource::Input {
                    id,
                    body: it.php_input.clone(),
                    pos: 0,
                    eof: false,
                    pos_broken: false,
                    uri: path.clone(),
                    mode: "rb".into(),
                    spilled_fd: None,
                    srbuf: Default::default(),
                    rcap: 0,
                    fraw: 0,
                })))
            } else if let Some(body) = parse_data_uri(&path) {
                // `data:[mediatype][;base64],payload` — zend's RFC2397
                // wrapper: a temp-backed read-only stream (scalar_*
                // tests fopen a data: URL for test values).
                let id = it.next_res_id();
                Value::Resource(Rc::new(RefCell::new(PhpResource::Input {
                    id,
                    body: Rc::new(body),
                    pos: 0,
                    eof: false,
                    pos_broken: false,
                    uri: path.clone(),
                    mode: mode.clone(),
                    spilled_fd: None,
                    srbuf: Default::default(),
                    rcap: 0,
                    fraw: 0,
                })))
            } else if let Some(mem) = mem_uri_kind(&path) {
                // php://memory and php://temp are always read/write
                // internally; fwrite still honors the fopen mode.
                let temp_smax = match mem {
                    MemUri::Temp(smax) => Some(smax),
                    MemUri::Memory => None,
                    MemUri::NegMax => {
                        return err(
                            "ValueError",
                            "fopen(): Argument #2 ($mode) must be greater than or equal to 0",
                        );
                    }
                };
                let id = it.next_res_id();
                if temp_smax.is_some() {
                    // zend's php://temp registers TWO streams — the
                    // outer temp stream + its inner memory stream
                    // (php_stream_temp_create allocs both) — burn the
                    // inner's res id so numbering stays aligned.
                    let _inner = it.next_res_id();
                }
                let w = mem_writeable(&mode);
                // zend reports the URI verbatim and a normalized mode:
                // '+' → w+b (a → a+b), else w → w+b, a → a+b, rest → rb.
                let meta_mode = if mode.contains('+') {
                    if mode.contains('a') {
                        "a+b"
                    } else {
                        "w+b"
                    }
                } else {
                    match mode.chars().next() {
                        Some('w') => "w+b",
                        Some('a') => "a+b",
                        _ => "rb",
                    }
                };
                Value::Resource(Rc::new(RefCell::new(PhpResource::Mem {
                    id,
                    buf: Vec::new(),
                    pos: 0,
                    eof: false,
                    pos_broken: false,
                    write: w,
                    append: mode.contains('a'),
                    uri: path.clone(),
                    mode: meta_mode.to_string(),
                    temp_smax,
                    spilled_fd: None,
                    srbuf: Default::default(),
                    rcap: 0,
                    fraw: 0,
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
                Value::Resource(Rc::new(RefCell::new(PhpResource::Stdio {
                    id,
                    which,
                    pos: 0,
                })))
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
                                rbuf: Default::default(),
                                rcap: 0,
                                unlink_on_close: false,
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
                    // PHP_STREAM_FLAG_NO_FCLOSE — an fclose() issued
                    // from inside a php_user_filter::filter() callback
                    // on this stream warns and fails (zend).
                    if it.filter_no_fclose == Some(id) {
                        it.warn_pub(
                            "fclose(): cannot close the provided stream, as it must not be manually closed",
                        )?;
                        return Ok(Some(Value::Bool(false)));
                    }
                    // zend's stream dtor flushes the WRITE chain once
                    // with empty input + the closing flag, writes the
                    // trailing bytes, then calls onClose on every
                    // php_user_filter (userfilter_dtor).
                    let mut close_err: Option<PhpError> = None;
                    if let Some(filters) = it.stream_filters.get(&id).cloned() {
                        if filters.iter().any(|f| f.write) {
                            drop(rb);
                            // An explicit fclose flushes while the
                            // stream zval is still live — ->stream is
                            // the real resource on this call (zend
                            // only NULLs it at request-shutdown frees).
                            // A filter exception still frees the chain
                            // (php_stream_free completes under a pending
                            // exception): capture it, tear down below,
                            // then re-raise so shutdown does not re-run
                            // the close.
                            let sv = Value::Resource(r.clone());
                            match run_filter_chain(
                                it,
                                id,
                                &sv,
                                false,
                                Vec::new(),
                                true,
                                false,
                                &mut None,
                            ) {
                                Ok((out, _)) => {
                                    if !out.is_empty() {
                                        let _ = write_resource_raw(it, r, &out);
                                    }
                                }
                                Err(e) => close_err = Some(e),
                            }
                            rb = r.borrow_mut();
                        }
                        let onclose: Vec<_> = filters
                            .iter()
                            .filter_map(|f| match &f.state {
                                FilterState::User(obj) => Some(obj.clone()),
                                _ => None,
                            })
                            .collect();
                        // onClose is user code — it may call back into
                        // stream builtins, so the stream borrow must
                        // not be held across it.
                        drop(rb);
                        for obj in onclose {
                            let _ = it.method_invoke(obj, "onClose", CallArgs::empty());
                        }
                        rb = r.borrow_mut();
                    }
                    *rb = PhpResource::Closed { id };
                    drop(rb);
                    // zend's stream dtor frees the attached filter
                    // chain: held filter resources go 'of type
                    // (Unknown)' and stream_filter_remove() TypeErrors.
                    if let Some(dead) = it.stream_filters.remove(&id) {
                        for f in dead {
                            it.codec_states.remove(&(f.fid, f.read));
                        }
                    }
                    let dead: Vec<u64> = it
                        .stream_filter_bindings
                        .iter()
                        .filter(|(_, (sid, _, _, _))| *sid == id)
                        .map(|(fid, _)| *fid)
                        .collect();
                    for fid in dead {
                        if let Some((_, _, fres, _)) = it.stream_filter_bindings.remove(&fid) {
                            let mut fb = fres.borrow_mut();
                            *fb = PhpResource::Closed { id: fid };
                        }
                    }
                    if let Some(e) = close_err {
                        return Err(e);
                    }
                }
            }
            Value::Bool(true)
        }
        "fwrite" | "fputs" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            // Byte-faithful: PHP strings are byte arrays — binary data
            // must survive the write unchanged.
            let data = arg_bs(it, args, 1);
            match write_resource(it, args.first(), &data)? {
                StreamWrite::Written => Value::Int(data.len() as i64),
                StreamWrite::Partial(n) => Value::Int(n as i64),
                StreamWrite::Ebadf(errno, msg) => {
                    write_ebadf_notice(it, name, data.len(), errno, &msg)?;
                    Value::Bool(false)
                }
                StreamWrite::NotWritable => {
                    it.notice_pub(&format!("{}(): Stream is not writable", name))?;
                    Value::Bool(false)
                }
                StreamWrite::Discarded => Value::Bool(false),
            }
        }
        "fread" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            let n = arg(args, 1).to_int().max(0) as usize;
            match read_resource(it, args.first(), n)? {
                StreamRead::Data(b) => Value::bytes(b),
                StreamRead::FailSilent => Value::Bool(false),
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
            match read_line_resource(it, args.first(), limit)? {
                StreamRead::FailSilent => return Ok(Some(Value::Bool(false))),
                StreamRead::Data(b) => {
                    if b.is_empty() {
                        Value::Bool(false)
                    } else {
                        Value::bytes(b)
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
            match read_resource(it, args.first(), 1)? {
                StreamRead::Data(b) if b.is_empty() => Value::Bool(false),
                StreamRead::Data(b) => Value::bytes(b),
                StreamRead::FailSilent => Value::Bool(false),
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
                    // zend's feof = stream->eof && the read buffer is
                    // drained — a filtered fill latches stream->eof
                    // while FILTERED bytes are still pending (the flag
                    // in meta['eof'] reports raw eof; feof gates it).
                    Value::Resource(r) => match &*r.borrow() {
                        PhpResource::File { eof, rbuf, .. } => Value::Bool(*eof && rbuf.is_empty()),
                        PhpResource::Mem { eof, srbuf, .. } => {
                            Value::Bool(*eof && srbuf.is_empty())
                        }
                        PhpResource::Pipe { eof, rbuf, .. } => Value::Bool(*eof && rbuf.is_empty()),
                        PhpResource::Input { eof, srbuf, .. } => {
                            Value::Bool(*eof && srbuf.is_empty())
                        }
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
                    // zend's _php_stream_do_seek flushes the write chain
                    // (FLUSH_INC) before seeking.
                    stream_seek_flush(it, r)?;
                    match &mut *r.borrow_mut() {
                        PhpResource::File {
                            id,
                            file,
                            pos,
                            eof,
                            rbuf,
                            rcap,
                            ..
                        } => {
                            use std::os::fd::AsRawFd;
                            if file_seek(
                                it,
                                *id,
                                file.as_raw_fd(),
                                pos,
                                eof,
                                rbuf,
                                rcap,
                                offset,
                                whence,
                                name,
                            )? < 0
                            {
                                return Ok(Some(Value::Int(-1)));
                            }
                        }
                        PhpResource::Mem {
                            buf,
                            pos,
                            eof,
                            pos_broken,
                            temp_smax,
                            spilled_fd,
                            srbuf,
                            fraw,
                            ..
                        } => {
                            // temp_cast'd temp streams behave like the
                            // plain tmpfile they now wrap: real lseek(2)
                            // plus zend's in-buffer seek fast path.
                            if let Some(fd) = *spilled_fd {
                                let r =
                                    fd_stream_seek(fd, pos, pos_broken, eof, srbuf, offset, whence);
                                return Ok(Some(Value::Int(r)));
                            }
                            // zend _php_stream_seek + php_stream_memory_seek:
                            // literal SEEK_SET<0 fails in the generic layer
                            // BEFORE ops->seek — position untouched; a CUR/END
                            // underflow reaches the ops seek which parks
                            // fpos=0 while stream->position becomes -1
                            // (pos_broken): ftell→false, next IO at 0.
                            // php://temp delegates to its inner FILE stream —
                            // only a SEEK_END underflow reaches the inner
                            // memory seek and breaks the position; a CUR
                            // underflow early-fails in the generic layer
                            // (position untouched). php://memory breaks on
                            // either.
                            let memory = temp_smax.is_none();
                            let tell = if *pos_broken {
                                *pos as i64 - 1
                            } else {
                                *pos as i64
                            };
                            // CUR is converted to SET against stream->position
                            // (tell) in the generic layer, then the ops seek
                            // runs on ms->fpos (pos). zend's own formula
                            // `offset > ZEND_LONG_MAX - position ? MAX :
                            // position + offset` — NOT checked_add: with a
                            // broken position (-1) `MAX - (-1)` wraps to
                            // MIN, so EVERY CUR offset lands on MAX.
                            let len = buf.len() as i64;
                            let new_pos = match whence {
                                0 if offset < 0 => None,
                                0 => Some(offset),
                                1 => Some(if offset > i64::MAX.wrapping_sub(tell) {
                                    i64::MAX
                                } else {
                                    tell.wrapping_add(offset)
                                }),
                                2 => Some(if offset > i64::MAX.wrapping_sub(len) {
                                    i64::MAX
                                } else {
                                    len.wrapping_add(offset)
                                }),
                                _ => None,
                            };
                            match new_pos {
                                Some(np) if np >= 0 => {
                                    *pos = np as u64;
                                    *fraw = np as u64;
                                    srbuf.clear();
                                    *pos_broken = false;
                                    *eof = false;
                                }
                                _ => {
                                    // Only a real CUR/END underflow reaches
                                    // the ops seek; an invalid whence (or a
                                    // SET<0) fails in the generic layer
                                    // first — position untouched. php://temp
                                    // breaks only on END; php://memory on
                                    // both CUR and END.
                                    if whence == 2 || (whence == 1 && memory) {
                                        *pos = 0;
                                        *fraw = 0;
                                        srbuf.clear();
                                        *pos_broken = true;
                                    }
                                    return Ok(Some(Value::Int(-1)));
                                }
                            }
                        }
                        PhpResource::Input {
                            body,
                            pos,
                            eof,
                            pos_broken,
                            spilled_fd,
                            srbuf,
                            fraw,
                            ..
                        } => {
                            if let Some(fd) = *spilled_fd {
                                let r =
                                    fd_stream_seek(fd, pos, pos_broken, eof, srbuf, offset, whence);
                                return Ok(Some(Value::Int(r)));
                            }
                            // php://input rides zend's memory seek — same
                            // broken-position marker as php://memory, and
                            // the same wrapped ZEND_LONG_MAX saturation.
                            let tell = if *pos_broken {
                                *pos as i64 - 1
                            } else {
                                *pos as i64
                            };
                            let len = body.len() as i64;
                            let new_pos = match whence {
                                0 if offset < 0 => None,
                                0 => Some(offset),
                                1 => Some(if offset > i64::MAX.wrapping_sub(tell) {
                                    i64::MAX
                                } else {
                                    tell.wrapping_add(offset)
                                }),
                                2 => Some(if offset > i64::MAX.wrapping_sub(len) {
                                    i64::MAX
                                } else {
                                    len.wrapping_add(offset)
                                }),
                                _ => None,
                            };
                            match new_pos {
                                Some(np) if np >= 0 => {
                                    *pos = np as u64;
                                    *fraw = np as u64;
                                    srbuf.clear();
                                    *pos_broken = false;
                                    *eof = false;
                                }
                                _ => {
                                    // Only a SEEK_END underflow reaches the
                                    // inner seek and breaks the position;
                                    // CUR/SET/invalid-whence failures leave
                                    // it untouched.
                                    if whence == 2 {
                                        *pos = 0;
                                        *fraw = 0;
                                        srbuf.clear();
                                        *pos_broken = true;
                                    }
                                    return Ok(Some(Value::Int(-1)));
                                }
                            }
                        }
                        PhpResource::Pipe {
                            file,
                            pos,
                            eof,
                            nonblock,
                            id,
                            rbuf,
                            ..
                        } => {
                            if whence == 1 && offset >= 0 {
                                // _php_stream_seek emulates forward SEEK_CUR
                                // on unseekable streams by read-and-discard;
                                // EOF/error mid-discard → silent -1.
                                let chunk = stream_chunk(it, *id);
                                let mut remaining = offset;
                                while remaining > 0 {
                                    match read_pipe(
                                        file,
                                        eof,
                                        *nonblock,
                                        pos,
                                        rbuf,
                                        chunk,
                                        (remaining as usize).min(chunk),
                                    )? {
                                        StreamRead::Data(b) if b.is_empty() => {
                                            return Ok(Some(Value::Int(-1)))
                                        }
                                        StreamRead::Data(b) => remaining -= b.len() as i64,
                                        StreamRead::FailSilent => return Ok(Some(Value::Int(-1))),
                                        StreamRead::Ebadf(errno, msg) => {
                                            read_ebadf_notice(it, name, errno, &msg)?;
                                            return Ok(Some(Value::Int(-1)));
                                        }
                                    }
                                }
                                *eof = false;
                            } else {
                                it.warn_pub(&format!(
                                    "{}(): Stream does not support seeking",
                                    name
                                ))?;
                                return Ok(Some(Value::Int(-1)));
                            }
                        }
                        PhpResource::Stdio { which, .. } => {
                            match stdio_fseek(it, *which, offset, whence, name)? {
                                StdioSeek::Ok => {}
                                StdioSeek::Fail => return Ok(Some(Value::Int(-1))),
                            }
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
                        PhpResource::File { pos, .. } => Value::Int(*pos as i64),
                        PhpResource::Mem {
                            pos, pos_broken, ..
                        }
                        | PhpResource::Input {
                            pos, pos_broken, ..
                        } => {
                            // zend stream->position is -1 after a failed
                            // memory seek — ftell reports false; the data
                            // cursor (pos) kept advancing through IO.
                            let t = if *pos_broken {
                                *pos as i64 - 1
                            } else {
                                *pos as i64
                            };
                            if t < 0 {
                                Value::Bool(false)
                            } else {
                                Value::Int(t)
                            }
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
                        PhpResource::Stdio { which, pos, .. } => match *which {
                            // ftell = lseek(fd,0,SEEK_CUR): ESPIPE on a
                            // pipe/socket → bool(false).
                            0..=2 => {
                                let r = unsafe { libc::lseek(*which as i32, 0, libc::SEEK_CUR) };
                                if r < 0 {
                                    Value::Bool(false)
                                } else {
                                    Value::Int(r as i64)
                                }
                            }
                            // php://output counts bytes written.
                            _ => Value::Int(*pos as i64),
                        },
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
                    stream_seek_flush(it, r)?;
                    match &mut *r.borrow_mut() {
                        PhpResource::File {
                            id,
                            file,
                            pos,
                            eof,
                            rbuf,
                            rcap,
                            ..
                        } => {
                            use std::os::fd::AsRawFd;
                            if file_seek(
                                it,
                                *id,
                                file.as_raw_fd(),
                                pos,
                                eof,
                                rbuf,
                                rcap,
                                0,
                                0,
                                name,
                            )? < 0
                            {
                                return Ok(Some(Value::Bool(false)));
                            }
                        }
                        PhpResource::Mem {
                            pos,
                            eof,
                            pos_broken,
                            spilled_fd,
                            srbuf,
                            fraw,
                            ..
                        }
                        | PhpResource::Input {
                            pos,
                            eof,
                            pos_broken,
                            spilled_fd,
                            srbuf,
                            fraw,
                            ..
                        } => {
                            *pos = 0;
                            *fraw = 0;
                            *pos_broken = false;
                            *eof = false;
                            srbuf.clear();
                            if let Some(fd) = *spilled_fd {
                                unsafe {
                                    libc::lseek(fd, 0, libc::SEEK_SET);
                                }
                            }
                        }
                        PhpResource::Pipe { .. } => {
                            it.warn_pub(&format!("{}(): Stream does not support seeking", name))?;
                            return Ok(Some(Value::Bool(false)));
                        }
                        PhpResource::Stdio { which, .. } => {
                            match stdio_fseek(it, *which, 0, 0, name)? {
                                StdioSeek::Ok => {}
                                StdioSeek::Fail => return Ok(Some(Value::Bool(false))),
                            }
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
                        PhpResource::Mem {
                            buf,
                            pos,
                            spilled_fd,
                            ..
                        } => Value::Bool(match spilled_fd {
                            // post-cast the buffer is inert — ftruncate(2)
                            // hits the real tmpfile (fd offset untouched).
                            Some(fd) => unsafe { libc::ftruncate(*fd, size as libc::off_t) == 0 },
                            None => {
                                buf.resize(size, 0);
                                if (*pos as usize) > size {
                                    *pos = size as u64;
                                }
                                true
                            }
                        }),
                        PhpResource::Input {
                            body,
                            pos,
                            spilled_fd,
                            ..
                        } => Value::Bool(match spilled_fd {
                            Some(fd) => unsafe { libc::ftruncate(*fd, size as libc::off_t) == 0 },
                            // data:/php://input are memory-backed streams —
                            // zend's temp set_option truncates the buffer.
                            None => {
                                let body = Rc::make_mut(body);
                                body.resize(size, 0);
                                if (*pos as usize) > size {
                                    *pos = size as u64;
                                }
                                true
                            }
                        }),
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
        "fflush" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            if let Value::Resource(r) = arg(args, 0) {
                let rid = r.borrow().id();
                // zend's php_stream_flush drives the write-filter
                // chain once with an empty brigade and closing=false
                // (PSFS_FLAG_FLUSH_NORMAL), then the stream's own
                // flush op — our writes are already unbuffered.
                if let Some(filters) = it.stream_filters.get(&rid).cloned() {
                    if filters.iter().any(|f| f.write) {
                        let sv = Value::Resource(r.clone());
                        let (out, _) = run_filter_chain(
                            it,
                            rid,
                            &sv,
                            false,
                            Vec::new(),
                            false,
                            true,
                            &mut None,
                        )?;
                        if !out.is_empty() {
                            let _ = write_resource_raw(it, &r, &out)?;
                        }
                    }
                }
            }
            Value::Bool(true)
        }
        "flock" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            // zend: flock(2) exists only on fd-backed streams — real
            // files, proc_open pipe ends, stdio — plus TEMP/RFC2397
            // streams AFTER a select/proc cast spilled them to a
            // tmpfile. Buffer-backed streams (php://memory, php://input,
            // php://output, pre-spill temp/data) have no lock support
            // and return false. PHP's LOCK_* constants are NOT libc's:
            // LOCK_SH/EX/UN/NB = 1/2/3/4 and zend accepts the op iff a
            // lock-mode bit (op & 3) is present — LOCK_UN(3) alone is
            // legal, stray bits (-1, 99) pass through, a bare NB(4)
            // or empty flags are ValueError.
            let op = zpp_long_arg(it, args, 1, name, 2, "$operation")?;
            if op & 3 == 0 {
                return err(
                    "ValueError",
                    format!(
                        "{}(): Argument #2 ($operation) must be one of LOCK_SH, LOCK_EX, or LOCK_UN",
                        name
                    ),
                );
            }
            let mut lop = if op & 3 == 3 {
                libc::LOCK_UN
            } else {
                (if op & 1 != 0 { libc::LOCK_SH } else { 0 })
                    | (if op & 2 != 0 { libc::LOCK_EX } else { 0 })
            };
            if op & 4 != 0 {
                lop |= libc::LOCK_NB;
            }
            let v = arg(args, 0);
            let fd = match &v {
                Value::Resource(r) => existing_fd(&r.borrow()),
                _ => None,
            };
            // &$wouldblock: 0 on success and on no-lock-support
            // streams, 1 only when the lock call itself would block.
            let (ret, wouldblock) = match fd {
                Some(fd) => {
                    let ok = unsafe { libc::flock(fd, lop) } == 0;
                    let wb = if ok {
                        0
                    } else {
                        (unsafe { *libc::__errno_location() } == libc::EWOULDBLOCK) as i64
                    };
                    (ok, wb)
                }
                None => (false, 0),
            };
            if let Some(c) = args.get(2) {
                *c.borrow_mut() = Value::Int(wouldblock);
            }
            Value::Bool(ret)
        }
        "fpassthru" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            let mut out = Vec::new();
            let mut failed = false;
            loop {
                match read_resource(it, args.first(), 8192)? {
                    StreamRead::Data(b) if b.is_empty() => break,
                    StreamRead::Data(b) => out.extend_from_slice(&b),
                    StreamRead::FailSilent => {
                        failed = true;
                        break;
                    }
                    StreamRead::Ebadf(errno, msg) => {
                        read_ebadf_notice(it, name, errno, &msg)?;
                        failed = true;
                        break;
                    }
                }
            }
            it.emit_bytes(&out);
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
                        PhpResource::File {
                            id,
                            file,
                            pos,
                            eof,
                            rbuf,
                            rcap,
                            ..
                        } => {
                            use std::os::fd::AsRawFd;
                            let _ = file_seek(
                                it,
                                *id,
                                file.as_raw_fd(),
                                pos,
                                eof,
                                rbuf,
                                rcap,
                                offset,
                                0,
                                name,
                            )?;
                        }
                        PhpResource::Mem {
                            pos,
                            eof,
                            pos_broken,
                            spilled_fd,
                            srbuf,
                            fraw,
                            ..
                        }
                        | PhpResource::Input {
                            pos,
                            eof,
                            pos_broken,
                            spilled_fd,
                            srbuf,
                            fraw,
                            ..
                        } => {
                            if let Some(fd) = *spilled_fd {
                                if fd_stream_seek(fd, pos, pos_broken, eof, srbuf, offset, 0) < 0 {
                                    *pos = offset as u64;
                                    *pos_broken = false;
                                    *eof = false;
                                }
                            } else {
                                *pos = offset as u64;
                                *fraw = offset as u64;
                                srbuf.clear();
                                *pos_broken = false;
                                *eof = false;
                            }
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
                match read_resource(it, args.first(), remaining.min(8192))? {
                    StreamRead::Data(b) if b.is_empty() => break,
                    StreamRead::Data(b) => {
                        remaining = remaining.saturating_sub(b.len());
                        out.extend_from_slice(&b);
                    }
                    StreamRead::FailSilent => break,
                    StreamRead::Ebadf(errno, msg) => {
                        read_ebadf_notice(it, name, errno, &msg)?;
                        break;
                    }
                }
            }
            Value::bytes(out)
        }
        "stream_copy_to_stream" => {
            stream_open_check(args, 0, name, 1, "from")?;
            stream_open_check(args, 1, name, 2, "to")?;
            let maxlen = args.get(2).map(|c| c.borrow().to_int()).unwrap_or(-1);
            let offset = args.get(3).map(|c| c.borrow().to_int()).unwrap_or(0);
            if offset > 0 {
                if let Some(Value::Resource(r)) = args.first().map(|c| c.borrow().clone()) {
                    match &mut *r.borrow_mut() {
                        PhpResource::File {
                            id,
                            file,
                            pos,
                            eof,
                            rbuf,
                            rcap,
                            ..
                        } => {
                            use std::os::fd::AsRawFd;
                            let _ = file_seek(
                                it,
                                *id,
                                file.as_raw_fd(),
                                pos,
                                eof,
                                rbuf,
                                rcap,
                                offset,
                                0,
                                name,
                            )?;
                        }
                        PhpResource::Mem {
                            pos,
                            eof,
                            pos_broken,
                            spilled_fd,
                            srbuf,
                            fraw,
                            ..
                        }
                        | PhpResource::Input {
                            pos,
                            eof,
                            pos_broken,
                            spilled_fd,
                            srbuf,
                            fraw,
                            ..
                        } => {
                            if let Some(fd) = *spilled_fd {
                                if fd_stream_seek(fd, pos, pos_broken, eof, srbuf, offset, 0) < 0 {
                                    *pos = offset as u64;
                                    *pos_broken = false;
                                    *eof = false;
                                }
                            } else {
                                *pos = offset as u64;
                                *fraw = offset as u64;
                                srbuf.clear();
                                *pos_broken = false;
                                *eof = false;
                            }
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
                match read_resource(it, args.first(), want)? {
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
                            StreamWrite::NotWritable => {
                                it.notice_pub(&format!("{}(): Stream is not writable", name))?;
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
                    StreamRead::FailSilent => {
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
            let v = arg(args, 0);
            if name == "posix_isatty" {
                // posix_isatty takes `resource|int $file_descriptor` —
                // a raw fd answers isatty directly. zend's union zpp
                // coerces scalars to int: bool/int silently, an
                // in-range float or well-formed float-string with a
                // "loses precision" deprecation, null deprecated to 0.
                // Incompatible (or out-of-i64-range) values fall to the
                // "int|resource, T given" warning; a coerced int outside
                // 0..=2147483647 warns "must be between" and fails.
                let long_ok =
                    |f: f64| f.is_finite() && f >= i64::MIN as f64 && f < -(i64::MIN as f64);
                let as_int: Option<i64> = match &v {
                    Value::Int(i) => Some(*i),
                    Value::Bool(b) => Some(*b as i64),
                    Value::Float(f) if long_ok(*f) => {
                        if f.fract() != 0.0 {
                            it.deprecated_pub(&format!(
                                "Implicit conversion from float {} to int loses precision",
                                crate::value::format_float_repr(*f)
                            ))?;
                        }
                        Some(*f as i64)
                    }
                    Value::Str(s) => match crate::value::numeric(s) {
                        crate::value::Numeric::Int(i) => Some(i),
                        crate::value::Numeric::Float(f) if long_ok(f) => {
                            if f.fract() != 0.0 {
                                it.deprecated_pub(&format!(
                                    "Implicit conversion from float-string \"{}\" to int loses precision",
                                    String::from_utf8_lossy(s)
                                ))?;
                            }
                            Some(f as i64)
                        }
                        _ => None,
                    },
                    Value::Null => {
                        it.deprecated_pub(&format!(
                            "{}(): Passing null to parameter #1 ($file_descriptor) of type int is deprecated",
                            name
                        ))?;
                        Some(0)
                    }
                    _ => None,
                };
                match (as_int, &v) {
                    (Some(i), _) => {
                        if !(0..=2147483647).contains(&i) {
                            it.warn_pub(&format!(
                                "{}(): Argument #1 ($file_descriptor) must be between 0 and 2147483647",
                                name
                            ))?;
                            return Ok(Some(Value::Bool(false)));
                        }
                        return Ok(Some(Value::Bool(unsafe { libc::isatty(i as i32) } == 1)));
                    }
                    (_, Value::Resource(_)) => {}
                    (_, other) => {
                        it.warn_pub(&format!(
                            "{}(): Argument #1 ($file_descriptor) must be of type int|resource, {} given",
                            name,
                            zval_word(other)
                        ))?;
                        return Ok(Some(Value::Bool(false)));
                    }
                }
            } else {
                stream_open_check(args, 0, name, 1, "stream")?;
            }
            // zend: isatty(3) on the stream's descriptor — true only
            // for fd-backed streams sitting on a tty (pty pipe ends,
            // real tty stdio). Buffer-backed streams are false.
            let (fd, label) = match &v {
                Value::Resource(r) => {
                    let rb = r.borrow();
                    match &*rb {
                        PhpResource::Proc { .. }
                        | PhpResource::Closed { .. }
                        | PhpResource::Other { .. } => {
                            // posix_isatty's own fetch failure message.
                            return err(
                                "TypeError",
                                format!(
                                    "{}(): supplied resource is not a valid stream resource",
                                    name
                                ),
                            );
                        }
                        _ => (existing_fd(&rb), stream_ops_label(&rb)),
                    }
                }
                _ => (None, ""),
            };
            match fd {
                Some(fd) => Value::Bool(unsafe { libc::isatty(fd) } == 1),
                None => {
                    if name == "posix_isatty" {
                        it.warn_pub(&format!(
                            "{}(): Could not use stream of type '{}'",
                            name, label
                        ))?;
                    }
                    Value::Bool(false)
                }
            }
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
        "stream_set_timeout" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            if args.len() > 1 {
                zpp_long_arg(it, args, 1, name, 2, "$seconds")?;
            }
            if args.len() > 2 {
                zpp_long_arg(it, args, 2, name, 3, "$microseconds")?;
            }
            // zend's socket_set_option stub: only socket-backed
            // streams honor a timeout; every other stream returns
            // false.
            let ok = match arg(args, 0) {
                Value::Resource(r) => {
                    matches!(&*r.borrow(), PhpResource::Pipe { socket: true, .. })
                }
                _ => false,
            };
            Value::Bool(ok)
        }
        "stream_set_read_buffer" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            if args.len() > 1 {
                zpp_long_arg(it, args, 1, name, 2, "$size")?;
            }
            Value::Int(0)
        }
        "stream_set_write_buffer" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            if args.len() > 1 {
                zpp_long_arg(it, args, 1, name, 2, "$size")?;
            }
            Value::Int(-1)
        }
        "stream_set_chunk_size" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            let new = zpp_long_arg(it, args, 1, name, 2, "$size")?;
            // zend returns the PREVIOUS chunk size (default 8192) and
            // installs the new one per stream.
            let (id, prev) = match arg(args, 0) {
                Value::Resource(r) => {
                    let id = r.borrow().id();
                    (id, it.stream_chunk_sizes.get(&id).copied().unwrap_or(8192))
                }
                _ => (0, 8192),
            };
            it.stream_chunk_sizes.insert(id, new);
            Value::Int(prev)
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
                            pty,
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
                                Value::str(if *socket || *pty {
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
                            rbuf,
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
                            base.push(("unread_bytes", Value::Int(rbuf.len() as i64)));
                            // zend reports the open-time seekability probe:
                            // false for fifo/chardev fd-backed streams.
                            base.push(("seekable", Value::Bool(stdio_seekable(file.as_raw_fd()))));
                            base.push(("uri", Value::str(path.clone())));
                            mk(base)
                        }
                        PhpResource::Stdio { which, .. } if *which <= 2 => {
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
                                    _ => "php://stdout",
                                }),
                            ));
                            mk(base)
                        }
                        PhpResource::Stdio { .. } => {
                            // php://output — the Output stream type.
                            base.push(("eof", Value::Bool(false)));
                            base.push(("wrapper_type", Value::str("PHP")));
                            base.push(("stream_type", Value::str("Output")));
                            base.push(("mode", Value::str("wb")));
                            base.push(("unread_bytes", Value::Int(0)));
                            base.push(("seekable", Value::Bool(false)));
                            base.push(("uri", Value::str("php://output")));
                            mk(base)
                        }
                        PhpResource::Mem {
                            eof,
                            uri,
                            mode,
                            temp_smax,
                            srbuf,
                            ..
                        } => {
                            // zend quirk: php://memory reports the full
                            // 9-key meta, but temp streams (TEMP) omit
                            // timed_out/blocked/eof entirely.
                            if temp_smax.is_none() {
                                base.push(("eof", Value::Bool(*eof)));
                            } else {
                                base.clear();
                            }
                            base.push(("wrapper_type", Value::str("PHP")));
                            base.push((
                                "stream_type",
                                Value::str(if temp_smax.is_none() {
                                    "MEMORY"
                                } else {
                                    "TEMP"
                                }),
                            ));
                            base.push(("mode", Value::str(mode.clone())));
                            // NO_BUFFER while unfiltered (0); once a read
                            // filter is attached the buffered FILTERED
                            // bytes are pending.
                            base.push(("unread_bytes", Value::Int(srbuf.len() as i64)));
                            base.push(("seekable", Value::Bool(true)));
                            base.push(("uri", Value::str(uri.clone())));
                            mk(base)
                        }
                        PhpResource::Input { eof, uri, mode, .. } if is_data_uri(uri) => {
                            // zend's RFC2397 meta: like TEMP there is
                            // no timed_out/blocked/eof, but the header
                            // mediatype/base64 keys lead the array and
                            // wrapper_type/stream_type are RFC2397.
                            let mut a = PhpArray::new();
                            let (mediatype, params, b64) = data_uri_meta(uri);
                            if let Some(mt) = mediatype {
                                a.set(ArrKey::Str("mediatype".into()), Value::str(mt));
                            }
                            for (k, v) in params {
                                a.set(ArrKey::Str(k.into()), Value::str(v));
                            }
                            a.set(ArrKey::Str("base64".into()), Value::Bool(b64));
                            a.set(ArrKey::Str("wrapper_type".into()), Value::str("RFC2397"));
                            a.set(ArrKey::Str("stream_type".into()), Value::str("RFC2397"));
                            a.set(ArrKey::Str("mode".into()), Value::str(mode.clone()));
                            a.set(ArrKey::Str("unread_bytes".into()), Value::Int(0));
                            a.set(ArrKey::Str("seekable".into()), Value::Bool(true));
                            a.set(ArrKey::Str("uri".into()), Value::str(uri.clone()));
                            Value::Array(Rc::new(RefCell::new(a)))
                        }
                        PhpResource::Input {
                            eof, uri, srbuf, ..
                        } => {
                            base.push(("eof", Value::Bool(*eof)));
                            base.push(("wrapper_type", Value::str("PHP")));
                            base.push(("stream_type", Value::str("Input")));
                            base.push(("mode", Value::str("rb")));
                            base.push(("unread_bytes", Value::Int(srbuf.len() as i64)));
                            base.push(("seekable", Value::Bool(true)));
                            base.push(("uri", Value::str(uri.clone())));
                            mk(base)
                        }
                        _ => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
                    }
                }
                _ => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
            }
        }
        "stream_get_filters" => {
            // zend's filter factory map in registration order — the
            // builtins plus every stream_filter_register() name.
            let mut out = PhpArray::new();
            for f in [
                "zlib.*",
                "bzip2.*",
                "convert.iconv.*",
                "string.rot13",
                "string.toupper",
                "string.tolower",
                "convert.*",
                "consumed",
                "dechunk",
            ] {
                out.push(Value::str(f));
            }
            for (n, _) in &it.user_filter_map {
                out.push(Value::str(n.clone()));
            }
            Value::Array(Rc::new(RefCell::new(out)))
        }
        "stream_get_wrappers" => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
        "stream_filter_register" => {
            if args.len() != 2 {
                return err(
                    "ArgumentCountError",
                    format!(
                        "stream_filter_register() expects exactly 2 arguments, {} given",
                        args.len()
                    ),
                );
            }
            let fname = zpp_strict_string(it, args, 0, name, "$filter_name")?;
            let cls = zpp_strict_string(it, args, 1, name, "$class")?;
            if fname.is_empty() {
                return err(
                    "ValueError",
                    "stream_filter_register(): Argument #1 ($filter_name) must be a non-empty string",
                );
            }
            if cls.is_empty() {
                return err(
                    "ValueError",
                    "stream_filter_register(): Argument #2 ($class) must be a non-empty string",
                );
            }
            // zend: user-map collision OR an exact builtin factory of
            // the same name (register_factory_volatile) → false.
            if it.user_filter_map.iter().any(|(n, _)| *n == fname)
                || builtin_factory(&fname).is_some()
            {
                return Ok(Some(Value::Bool(false)));
            }
            it.user_filter_map.push((fname, cls));
            Value::Bool(true)
        }
        "stream_filter_append" | "stream_filter_prepend" => {
            stream_filter_attach(it, args, name, name == "stream_filter_prepend")?
        }
        "stream_filter_remove" => stream_filter_remove(it, args)?,
        "stream_bucket_make_writeable" => {
            if args.len() != 1 {
                return err(
                    "ArgumentCountError",
                    format!(
                        "stream_bucket_make_writeable() expects exactly 1 argument, {} given",
                        args.len()
                    ),
                );
            }
            match arg(args, 0) {
                Value::Resource(r) => {
                    let rid = r.borrow().id();
                    let is_brig = matches!(
                        &*r.borrow(),
                        PhpResource::Other {
                            kind: "userfilter.bucket brigade",
                            ..
                        }
                    );
                    if !is_brig {
                        return err(
                            "TypeError",
                            "stream_bucket_make_writeable(): supplied resource is not a valid userfilter.bucket brigade resource",
                        );
                    }
                    match it.stream_brigades.get_mut(&rid).and_then(|b| b.pop_front()) {
                        Some(bytes) => new_stream_bucket(it, bytes)?,
                        // an empty brigade yields falsy (zend returns
                        // NULL — the var_dump/null-print path).
                        None => Value::Null,
                    }
                }
                v => {
                    return err(
                        "TypeError",
                        format!(
                            "stream_bucket_make_writeable(): Argument #1 ($brigade) must be of type resource, {} given",
                            zval_word(&v)
                        ),
                    );
                }
            }
        }
        "stream_bucket_append" | "stream_bucket_prepend" => {
            let append = name == "stream_bucket_append";
            if args.len() != 2 {
                return err(
                    "ArgumentCountError",
                    format!("{name}() expects exactly 2 arguments, {} given", args.len()),
                );
            }
            match arg(args, 0) {
                Value::Resource(r) => {
                    let rid = r.borrow().id();
                    let is_brig = matches!(
                        &*r.borrow(),
                        PhpResource::Other {
                            kind: "userfilter.bucket brigade",
                            ..
                        }
                    );
                    if !is_brig {
                        return err(
                            "TypeError",
                            format!(
                                "{name}(): supplied resource is not a valid userfilter.bucket brigade resource",
                            ),
                        );
                    }
                    let bval = arg(args, 1);
                    let Value::Object(bobj) = &bval else {
                        return err(
                            "TypeError",
                            format!(
                                "{name}(): Argument #2 ($bucket) must be of type StreamBucket, {} given",
                                zval_word(&bval)
                            ),
                        );
                    };
                    if !it.obj_is_a_str(bobj.borrow().class.name(), "streambucket") {
                        return err(
                            "TypeError",
                            format!(
                                "{name}(): Argument #2 ($bucket) must be of type StreamBucket, {} given",
                                zval_word(&bval)
                            ),
                        );
                    }
                    // a missing/invalid 'bucket' property:
                    // ValueError arg #2 'must be an object that has a
                    // "bucket" property'.
                    let has_bucket = bobj
                        .borrow()
                        .props
                        .get("bucket")
                        .map(|c| matches!(&*c.borrow(), Value::Resource(_)))
                        .unwrap_or(false);
                    if !has_bucket {
                        return err(
                            "ValueError",
                            format!(
                                "{name}(): Argument #2 ($bucket) must be an object that has a \"bucket\" property",
                            ),
                        );
                    }
                    // zend memcpy's the 'data' prop over the bucket's
                    // bytes — userland edits to ->data take effect.
                    let bytes = it.to_bytes_of(
                        &bobj
                            .borrow()
                            .props
                            .get("data")
                            .map(|c| c.borrow().clone())
                            .unwrap_or(Value::Str(Vec::new().into())),
                    );
                    if append {
                        it.stream_brigades.entry(rid).or_default().push_back(bytes);
                    } else {
                        it.stream_brigades.entry(rid).or_default().push_front(bytes);
                    }
                    Value::Null
                }
                v => {
                    return err(
                        "TypeError",
                        format!(
                            "{name}(): Argument #1 ($brigade) must be of type resource, {} given",
                            zval_word(&v)
                        ),
                    );
                }
            }
        }
        "stream_bucket_new" => {
            if args.len() != 2 {
                return err(
                    "ArgumentCountError",
                    format!(
                        "stream_bucket_new() expects exactly 2 arguments, {} given",
                        args.len()
                    ),
                );
            }
            match arg(args, 0) {
                Value::Resource(r) => {
                    // zend PHP_Z_PARAM_STREAM — a non-stream resource
                    // (a filter or brigade handle) is a TypeError.
                    let streamish = !matches!(
                        &*r.borrow(),
                        PhpResource::Closed { .. }
                            | PhpResource::Proc { .. }
                            | PhpResource::Other { .. }
                    );
                    if !streamish {
                        return err(
                            "TypeError",
                            "stream_bucket_new(): supplied resource is not a valid stream resource",
                        );
                    }
                    let bytes = zpp_strict_string(it, args, 1, name, "$buffer")?;
                    new_stream_bucket(it, bytes.into_bytes())?
                }
                v => {
                    return err(
                        "TypeError",
                        format!(
                            "stream_bucket_new(): Argument #1 ($stream) must be of type resource, {} given",
                            zval_word(&v)
                        ),
                    );
                }
            }
        }
        "fstat" => {
            stream_open_check(args, 0, name, 1, "stream")?;
            match arg(args, 0) {
                Value::Resource(r) => {
                    let arr = {
                        let res = r.borrow();
                        match &*res {
                            crate::value::PhpResource::File { file, .. } => {
                                file.metadata().ok().map(|m| stat_array(&m))
                            }
                            crate::value::PhpResource::Pipe { file, .. } => {
                                file.metadata().ok().map(|m| stat_array(&m))
                            }
                            crate::value::PhpResource::Stdio { which, .. } => match which {
                                0 => std::fs::metadata("/dev/stdin").ok().map(|m| stat_array(&m)),
                                1 => std::fs::metadata("/dev/stdout")
                                    .ok()
                                    .map(|m| stat_array(&m)),
                                2 => std::fs::metadata("/dev/stderr")
                                    .ok()
                                    .map(|m| stat_array(&m)),
                                // php://output has no fd — fstat fails.
                                _ => None,
                            },
                            // Buffer-backed streams answer zend's
                            // synthetic statbuf; after a FOR_SELECT cast
                            // spilled them to a tmpfile the REAL inode
                            // shows up (dev/ino of the spool file).
                            crate::value::PhpResource::Mem {
                                buf, spilled_fd, ..
                            } => Some(match spilled_fd {
                                Some(fd) => std::fs::metadata(format!("/proc/self/fd/{}", fd))
                                    .ok()
                                    .map(|m| stat_array(&m))
                                    .unwrap_or_else(|| buf_stat_array(buf.len())),
                                None => buf_stat_array(buf.len()),
                            }),
                            crate::value::PhpResource::Input {
                                uri,
                                body,
                                spilled_fd,
                                ..
                            } if is_data_uri(uri) => Some(match spilled_fd {
                                Some(fd) => std::fs::metadata(format!("/proc/self/fd/{}", fd))
                                    .ok()
                                    .map(|m| stat_array(&m))
                                    .unwrap_or_else(|| buf_stat_array(body.len())),
                                None => buf_stat_array(body.len()),
                            }),
                            // php://input is not stat-able in zend.
                            _ => None,
                        }
                    };
                    match arr {
                        Some(a) => Value::Array(Rc::new(RefCell::new(a))),
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

/// The RFC2397 header of a data: URI as zend reports it in
/// stream_get_meta_data(): (mediatype — text before the first ';',
/// None when empty —, the ';'-separated `attr=val` params in order
/// as their own meta keys, the base64 flag). Only an exact `;base64`
/// sets the flag; a `mediatype=` param never surfaces (zend skips it),
/// and `base64=…` params collapse into the bool zend appends last.
fn data_uri_meta(path: &str) -> (Option<String>, Vec<(String, String)>, bool) {
    let rest = path
        .strip_prefix("data:")
        .or_else(|| path.strip_prefix("data://"))
        .unwrap_or("");
    let rest = rest.strip_prefix("//").unwrap_or(rest);
    let meta = rest.split(',').next().unwrap_or("");
    let mut segs = meta.split(';');
    let mediatype = segs.next().unwrap_or("");
    let mut params = Vec::new();
    let mut b64 = false;
    for seg in segs {
        match seg.split_once('=') {
            Some((k, v)) => {
                // zend assoc_adds each param verbatim except one
                // literally named 'mediatype'; 'base64=v' is later
                // overwritten by the appended bool — same net effect
                // as skipping it here.
                if k != "mediatype" && k != "base64" {
                    params.push((k.to_string(), v.to_string()));
                }
            }
            None => {
                if seg == "base64" {
                    b64 = true;
                }
            }
        }
    }
    (
        if mediatype.is_empty() {
            None
        } else {
            Some(mediatype.to_string())
        },
        params,
        b64,
    )
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

/// The php:// URI kinds zend's php_stream_url_wrap_php maps to a
/// memory-backed stream: a case-insensitive "temp" PREFIX match
/// (php://tempxyz counts, php://tem does not) optionally followed by
/// "/maxmemory:" + ZEND_STRTOL, vs an exact case-insensitive
/// "memory". The "php://" scheme itself is compared
/// case-insensitively too and reported verbatim in meta 'uri'.
enum MemUri {
    /// php://temp* — the value is ts->smax: the /maxmemory:N budget,
    /// default PHP_STREAM_MAX_MEM = 2MB (spill on pos+count >= smax).
    Temp(u64),
    /// php://memory — never spills, not fd-castable.
    Memory,
    /// php://temp/maxmemory:<negative> → the caller's arg2 ValueError.
    NegMax,
}

fn mem_uri_kind(path: &str) -> Option<MemUri> {
    let b = path.as_bytes();
    if b.len() < 6 || !b[..6].eq_ignore_ascii_case(b"php://") {
        return None;
    }
    let rest = &b[6..];
    if rest.len() >= 4 && rest[..4].eq_ignore_ascii_case(b"temp") {
        let tail = &rest[4..];
        if tail.len() >= 11 && tail[..11].eq_ignore_ascii_case(b"/maxmemory:") {
            let v = parse_strtol(&tail[11..]);
            return Some(if v < 0 {
                MemUri::NegMax
            } else {
                MemUri::Temp(v as u64)
            });
        }
        Some(MemUri::Temp(2 * 1024 * 1024))
    } else if rest.eq_ignore_ascii_case(b"memory") {
        Some(MemUri::Memory)
    } else {
        None
    }
}

/// C strtol(…, 10): skips leading whitespace, an optional +/- sign,
/// then digits; a non-numeric tail is ignored, no digits → 0,
/// overflow saturates (zend ZEND_STRTOL on maxmemory).
fn parse_strtol(b: &[u8]) -> i64 {
    let mut i = 0;
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        i += 1;
    }
    let mut neg = false;
    match b.get(i) {
        Some(b'-') => {
            neg = true;
            i += 1;
        }
        Some(b'+') => {
            i += 1;
        }
        _ => {}
    }
    let mut v: i64 = 0;
    let mut any = false;
    while i < b.len() && b[i].is_ascii_digit() {
        any = true;
        v = v.saturating_mul(10).saturating_add((b[i] - b'0') as i64);
        i += 1;
    }
    if !any {
        0
    } else if neg {
        -v
    } else {
        v
    }
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
    /// Write refused by a stream that notices it isn't writable
    /// (zend's "Stream is not writable" E_NOTICE — RFC2397).
    NotWritable,
    Discarded,
}

/// Outcome of a `php_stream_read` attempt — same E_NOTICE contract on
/// a descriptor not open for reading.
pub(in crate::builtins) enum StreamRead {
    Data(Vec<u8>),
    Ebadf(i32, String),
    /// zend's 'no read op' streams (php://output): the read just
    /// returns failure with no diagnostic.
    FailSilent,
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
    match c.map(|c| c.borrow().clone()) {
        Some(Value::Resource(r)) => {
            let rid = r.borrow().id();
            // stream write filters transform the outgoing bytes —
            // zend's _php_stream_write_filtered runs the write chain
            // and ops->write receives the OUTPUT; fwrite() still
            // reports the input length.
            let mut owned: Option<Vec<u8>> = None;
            if stream_write_filtered(it, rid) {
                let sv = Value::Resource(r.clone());
                let (out, status) =
                    run_filter_chain(it, rid, &sv, false, data.to_vec(), false, false, &mut None)?;
                if status != 2 {
                    return Ok(StreamWrite::Discarded);
                }
                owned = Some(out);
            }
            write_resource_raw(it, &r, owned.as_deref().unwrap_or(data))
        }
        _ => Err(PhpError::fatal("not a resource", 0)),
    }
}

/// The ops->write half — bytes already filtered.
fn write_resource_raw(
    it: &mut Interp,
    r: &Rc<RefCell<PhpResource>>,
    data: &[u8],
) -> Result<StreamWrite, PhpError> {
    use std::io::Write;
    let mut rb = r.borrow_mut();
    match &mut *rb {
        PhpResource::Stdio { which, pos, .. } => match *which {
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
                // zend tracks bytes written for ftell().
                *pos += data.len() as u64;
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
            file,
            pos,
            write,
            rbuf,
            ..
        } => {
            if !*write {
                return Ok(StreamWrite::Ebadf(9, "Bad file descriptor".into()));
            }
            use std::os::fd::AsRawFd;
            // zend's buffered write resyncs the fd to the
            // logical position only when a userspace read
            // buffer still holds unread bytes on a seekable
            // stream (streams.c:1194) — otherwise the write
            // lands at the real fd offset, which a dup'd child
            // may have moved.
            if !rbuf.is_empty() && stdio_seekable(file.as_raw_fd()) {
                fd_resync(file.as_raw_fd(), *pos, rbuf);
            }
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
            append,
            spilled_fd,
            srbuf,
            temp_smax,
            ..
        } => {
            // Once the buffer spills (temp_cast or a write that
            // crossed smax) the stream io hits the inner r+b
            // tmpfile — writes land even on 'r'-mode php://temp
            // (the write flag only gates the in-buffer op).
            if let Some(fd) = *spilled_fd {
                // zend's buffered write discards the read buffer
                // and reseeks the fd to the logical position ONLY
                // when userspace-buffered bytes are pending
                // (streams.c:1194 readpos != writepos) — with an
                // empty buffer the write lands at the kernel's
                // leftover offset (e.g. after a proc child
                // consumed bytes).
                if !srbuf.is_empty() {
                    fd_resync(fd, *pos, srbuf);
                }
                return Ok(fd_stream_write(fd, pos, eof, data));
            }
            // zend php_stream_temp_write: the whole buffer
            // spills to a tmpfile once stream->position+count
            // reaches smax — BEFORE the inner memory stream's
            // readonly check, so a spilling write lands even
            // on 'r'-mode temps.
            if let Some(smax) = *temp_smax {
                if *pos as u128 + data.len() as u128 >= smax as u128 {
                    match temp_spill_fd(buf, *pos) {
                        Some(fd) => {
                            *spilled_fd = Some(fd);
                            return Ok(fd_stream_write(fd, pos, eof, data));
                        }
                        None => {
                            it.warn_pub("Unable to create temporary file, Check permissions in temporary files directory.")?;
                            return Ok(StreamWrite::Partial(0));
                        }
                    }
                }
            }
            // TEMP_STREAM_READONLY → php_stream_memory_write
            // returns -1 and the bytes are silently dropped
            // (no E_NOTICE — only plain stdio notices).
            if !*write {
                return Ok(StreamWrite::Discarded);
            }
            // TEMP_STREAM_APPEND: an 'a'-mode write lands at
            // end-of-buffer regardless of the current position.
            if *append {
                *pos = buf.len() as u64;
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
        // RFC2397 streams notice like a plain fd failing the
        // writable check; php://input drops silently.
        PhpResource::Input { uri, .. } if is_data_uri(uri) => Ok(StreamWrite::NotWritable),
        PhpResource::Input { .. } => Ok(StreamWrite::Discarded),
        _ => Err(PhpError::fatal("bad resource", 0)),
    }
}

/// zend's pipe fill size — the per-stream chunk from
/// stream_set_chunk_size(), default 8192.
fn stream_chunk(it: &Interp, id: u64) -> usize {
    it.stream_chunk_sizes
        .get(&id)
        .copied()
        .unwrap_or(8192)
        .max(1) as usize
}

/// php_stream_read on a pipe — zend's buffered model for non-file
/// streams: the call drains the read buffer and performs AT MOST one
/// fill_read_buffer of chunk_size bytes (so fread($p, 200000)
/// returns at most the chunk). A chunk_size of 1 makes the stream
/// unbuffered — one raw read of the whole remaining request. EOF
/// (fill 0) latches `eof`; EAGAIN on a nonblocking stream reads
/// "" silently.
fn read_pipe(
    file: &mut std::fs::File,
    eof: &mut bool,
    nonblock: bool,
    pos: &mut u64,
    rbuf: &mut std::collections::VecDeque<u8>,
    chunk: usize,
    n: usize,
) -> Result<StreamRead, PhpError> {
    use std::io::Read;
    let mut out: Vec<u8> = Vec::with_capacity(n.min(8192));
    out.extend(rbuf.drain(..n.min(rbuf.len())));
    if out.len() < n && !*eof {
        let want = if chunk == 1 { n - out.len() } else { chunk };
        let mut buf = vec![0u8; want];
        match file.read(&mut buf) {
            Ok(0) => *eof = true,
            Ok(got) => {
                buf.truncate(got);
                rbuf.extend(buf);
                let more = (n - out.len()).min(rbuf.len());
                out.extend(rbuf.drain(..more));
            }
            Err(e) if nonblock && e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                let (errno, msg) = io_errno_str(&e);
                return Ok(StreamRead::Ebadf(errno, msg));
            }
        }
    }
    *pos += out.len() as u64;
    Ok(StreamRead::Data(out))
}

/// One fill_read_buffer on a spilled (temp_cast'd) stream: the outer
/// readbuf grows one chunk_size whenever free space drops below the
/// chunk (zend's `readbuflen += chunk_size` after compaction), then
/// takes a single read(2) of the free span — the inner stdio stream
/// is transparent because it loops internally until it delivers the
/// whole outer request. A 0-byte fill latches eof; buffered bytes
/// already read keep serving after it.
fn fd_fill(
    fd: std::os::unix::io::RawFd,
    srbuf: &mut std::collections::VecDeque<u8>,
    rcap: &mut usize,
    chunk: usize,
    eof: &mut bool,
) -> Result<(), (i32, String)> {
    let mut free = rcap.saturating_sub(srbuf.len());
    if free < chunk {
        *rcap = rcap.saturating_add(chunk);
        free += chunk;
    }
    let mut buf = vec![0u8; free];
    let got = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, free) };
    if got < 0 {
        let e = std::io::Error::last_os_error();
        return Err(io_errno_str(&e));
    }
    if got == 0 {
        *eof = true;
    }
    buf.truncate(got as usize);
    srbuf.extend(&buf);
    Ok(())
}

/// php_stream_read on a spilled stream: drain the outer readbuf, then
/// fill+drain until the request is met or the fd reports EOF
/// (temp/memory ops are exempt from zend's single-fill break, so the
/// loop is greedy). A chunk_size of 1 flips zend's NO_BUFFER flag —
/// reads then bypass the readbuf entirely (stale bytes just sit).
fn fd_stream_read(
    fd: std::os::unix::io::RawFd,
    pos: &mut u64,
    eof: &mut bool,
    srbuf: &mut std::collections::VecDeque<u8>,
    rcap: &mut usize,
    chunk: usize,
    n: usize,
) -> StreamRead {
    let mut out: Vec<u8> = Vec::with_capacity(n.min(8192));
    if chunk != 1 {
        out.extend(srbuf.drain(..n.min(srbuf.len())));
    }
    while out.len() < n && !*eof {
        if chunk == 1 {
            let mut buf = vec![0u8; n - out.len()];
            let got = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if got <= 0 {
                if got < 0 {
                    let e = std::io::Error::last_os_error();
                    let (errno, msg) = io_errno_str(&e);
                    return StreamRead::Ebadf(errno, msg);
                }
                *eof = true;
                break;
            }
            buf.truncate(got as usize);
            out.extend_from_slice(&buf);
            continue;
        }
        if let Err((errno, msg)) = fd_fill(fd, srbuf, rcap, chunk, eof) {
            return StreamRead::Ebadf(errno, msg);
        }
        let take = (n - out.len()).min(srbuf.len());
        out.extend(srbuf.drain(..take));
    }
    *pos += out.len() as u64;
    StreamRead::Data(out)
}

/// php_stream_gets on a spilled stream — drains the outer readbuf and
/// fills in chunk_size spans until '\n', `limit`, or EOF.
fn fd_line_read(
    fd: std::os::unix::io::RawFd,
    pos: &mut u64,
    eof: &mut bool,
    srbuf: &mut std::collections::VecDeque<u8>,
    rcap: &mut usize,
    chunk: usize,
    limit: usize,
) -> StreamRead {
    let mut out = Vec::new();
    while out.len() < limit {
        if chunk != 1 {
            if let Some(b) = srbuf.pop_front() {
                out.push(b);
                if b == b'\n' {
                    break;
                }
                continue;
            }
        }
        if *eof {
            break;
        }
        if chunk == 1 {
            // unbuffered — zend reads one byte at a time, rbuf bypassed
            let mut byte = [0u8; 1];
            match unsafe { libc::read(fd, byte.as_mut_ptr() as *mut _, 1) } {
                0 => {
                    *eof = true;
                }
                n if n < 0 => {
                    let e = std::io::Error::last_os_error();
                    let (errno, msg) = io_errno_str(&e);
                    return StreamRead::Ebadf(errno, msg);
                }
                _ => {
                    out.push(byte[0]);
                    if byte[0] == b'\n' {
                        break;
                    }
                }
            }
            continue;
        }
        if let Err((errno, msg)) = fd_fill(fd, srbuf, rcap, chunk, eof) {
            return StreamRead::Ebadf(errno, msg);
        }
        if srbuf.is_empty() && *eof {
            break;
        }
    }
    *pos += out.len() as u64;
    StreamRead::Data(out)
}

/// php_stream_write on a spilled stream — write(2) at the fd's own
/// offset (the tmpfile innerstream is r+b stdio: even 'r'-mode
/// php://temp accepts writes once cast).
fn fd_stream_write(
    fd: std::os::unix::io::RawFd,
    pos: &mut u64,
    eof: &mut bool,
    data: &[u8],
) -> StreamWrite {
    let n = unsafe { libc::write(fd, data.as_ptr() as *const _, data.len()) };
    if n < 0 {
        let e = std::io::Error::last_os_error();
        let (errno, msg) = io_errno_str(&e);
        return StreamWrite::Ebadf(errno, msg);
    }
    *pos += n as u64;
    *eof = false;
    if n as usize == data.len() {
        StreamWrite::Written
    } else {
        StreamWrite::Partial(n as usize)
    }
}

/// fseek on a spilled stream — zend's _php_stream_seek: first the
/// in-buffer fast path (CUR/SET targets already inside the readbuf
/// consume bytes without touching the fd — no lseek at all), else the
/// generic layer converts CUR to an absolute SET on stream->position
/// (saturating, <0 fails untouched) and the inner stdio seek lseek(2)s
/// the fd — position becomes the new offset and the readbuf resets.
fn fd_stream_seek(
    fd: std::os::unix::io::RawFd,
    pos: &mut u64,
    pos_broken: &mut bool,
    eof: &mut bool,
    srbuf: &mut std::collections::VecDeque<u8>,
    offset: i64,
    whence: i64,
) -> i64 {
    let buffered = srbuf.len() as i64;
    let tell = if *pos_broken {
        *pos as i64 - 1
    } else {
        *pos as i64
    };
    // in-buffer fast path (the buffer lives in the inner stream,
    // which is always buffered — zend checks the flag, not chunk)
    match whence {
        1 if offset > 0 && offset <= buffered => {
            srbuf.drain(..offset as usize);
            *pos = tell.wrapping_add(offset) as u64;
            *pos_broken = false;
            *eof = false;
            return 0;
        }
        0 if offset > tell && offset <= tell + buffered => {
            let adv = (offset - tell) as usize;
            srbuf.drain(..adv.min(srbuf.len()));
            *pos = offset as u64;
            *pos_broken = false;
            *eof = false;
            return 0;
        }
        _ => {}
    }
    // generic layer: SEEK_CUR becomes SET against stream->position.
    let (target, w) = match whence {
        0 => (offset, libc::SEEK_SET),
        1 => {
            let t = if offset > i64::MAX.wrapping_sub(tell) {
                i64::MAX
            } else {
                tell.wrapping_add(offset)
            };
            (t, libc::SEEK_SET)
        }
        2 => (offset, libc::SEEK_END),
        _ => return -1,
    };
    if w == libc::SEEK_SET && target < 0 {
        return -1;
    }
    let r = unsafe { libc::lseek(fd, target as libc::off_t, w) };
    if r < 0 {
        // a seekable stream drops its readbuf on every real seek —
        // success or EINVAL alike (streams.c clears it unconditionally
        // when !NO_SEEK).
        srbuf.clear();
        -1
    } else {
        *pos = r as u64;
        *pos_broken = false;
        *eof = false;
        srbuf.clear();
        0
    }
}

/// fseek on a File stream — zend's _php_stream_seek for an fd-backed
/// stdio stream: in-buffer fast path first (CUR within the readbuf or
/// SET into it — runs even on unseekable streams), then for seekable
/// streams a real lseek(2) that drops the readbuf, else zend's
/// read-discard emulation for forward SEEK_CUR and the
/// 'does not support seeking' warn for the rest.
#[allow(clippy::too_many_arguments)]
fn file_seek(
    it: &mut Interp,
    id: u64,
    fd: std::os::unix::io::RawFd,
    pos: &mut u64,
    eof: &mut bool,
    rbuf: &mut std::collections::VecDeque<u8>,
    rcap: &mut usize,
    offset: i64,
    whence: i64,
    name: &str,
) -> Result<i64, PhpError> {
    let buffered = rbuf.len() as i64;
    let tell = *pos as i64;
    match whence {
        1 if offset > 0 && offset <= buffered => {
            rbuf.drain(..offset as usize);
            *pos = tell.wrapping_add(offset) as u64;
            *eof = false;
            return Ok(0);
        }
        0 if offset > tell && offset <= tell + buffered => {
            let adv = (offset - tell) as usize;
            rbuf.drain(..adv.min(rbuf.len()));
            *pos = offset as u64;
            *eof = false;
            return Ok(0);
        }
        _ => {}
    }
    if stdio_seekable(fd) {
        // generic layer: CUR rewrites to an absolute SET on position.
        let (target, w) = match whence {
            0 => (offset, libc::SEEK_SET),
            1 => {
                let t = if offset > i64::MAX.wrapping_sub(tell) {
                    i64::MAX
                } else {
                    tell.wrapping_add(offset)
                };
                (t, libc::SEEK_SET)
            }
            2 => (offset, libc::SEEK_END),
            _ => return Ok(-1),
        };
        if w == libc::SEEK_SET && target < 0 {
            return Ok(-1);
        }
        let r = unsafe { libc::lseek(fd, target as libc::off_t, w) };
        // a seekable stream drops its readbuf on every real seek —
        // success or EINVAL alike (zend clears it unconditionally
        // when !NO_SEEK).
        rbuf.clear();
        if r < 0 {
            Ok(-1)
        } else {
            *pos = r as u64;
            *eof = false;
            Ok(0)
        }
    } else if whence == 1 && offset >= 0 {
        // unseekable: zend emulates a forward CUR seek by reading.
        let chunk = stream_chunk(it, id);
        let mut remaining = offset;
        while remaining > 0 {
            match fd_stream_read(fd, pos, eof, rbuf, rcap, chunk, remaining as usize) {
                StreamRead::Data(b) if b.is_empty() => return Ok(-1),
                StreamRead::Data(b) => remaining -= b.len() as i64,
                StreamRead::FailSilent | StreamRead::Ebadf(..) => return Ok(-1),
            }
        }
        *eof = false;
        Ok(0)
    } else {
        it.warn_pub(&format!("{}(): Stream does not support seeking", name))?;
        Ok(-1)
    }
}

/// Whether a stdio fd is seekable — zend probes this at open time with
/// fstat: NOT seekable on FIFOs, nor on char devices except
/// /dev/{null,zero,full,random,urandom,tty} (chrdev major 1, minors
/// 1,2,3,4,5,7).
pub(in crate::builtins) fn stdio_seekable(fd: libc::c_int) -> bool {
    unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd, &mut st) != 0 {
            return false;
        }
        if (st.st_mode & libc::S_IFMT) == libc::S_IFIFO {
            return false;
        }
        if (st.st_mode & libc::S_IFMT) == libc::S_IFCHR {
            return libc::major(st.st_rdev) == 1
                && matches!(libc::minor(st.st_rdev), 1 | 2 | 3 | 4 | 5 | 7);
        }
        true
    }
}

enum StdioSeek {
    Ok,
    Fail,
}

/// fseek/rewind on php://stdin|stdout|stderr|output — zend's stdio ops:
/// on a seekable fd a real lseek (failure is a silent -1, position
/// untouched); on a NO_SEEK stream _php_stream_seek emulates a forward
/// SEEK_CUR by read-and-discard (0 on success, silent -1 on EOF/error)
/// and warns "Stream does not support seeking" for anything else.
fn stdio_fseek(
    it: &mut Interp,
    which: u8,
    offset: i64,
    whence: i64,
    fname: &str,
) -> Result<StdioSeek, PhpError> {
    if which > 2 {
        // php://output — output-buffer chain, not a real fd: the zend
        // output stream has no read op, so the discard loop sees
        // didread<=0 immediately. A zero-length CUR seek still "wins".
        if whence == 1 && offset >= 0 {
            return Ok(if offset == 0 {
                StdioSeek::Ok
            } else {
                StdioSeek::Fail
            });
        }
        it.warn_pub(&format!("{}(): Stream does not support seeking", fname))?;
        return Ok(StdioSeek::Fail);
    }
    let fd = which as libc::c_int;
    if stdio_seekable(fd) {
        let w = match whence {
            0 => libc::SEEK_SET,
            1 => libc::SEEK_CUR,
            2 => libc::SEEK_END,
            _ => return Ok(StdioSeek::Fail),
        };
        return Ok(
            if unsafe { libc::lseek(fd, offset as libc::off_t, w) } < 0 {
                StdioSeek::Fail
            } else {
                StdioSeek::Ok
            },
        );
    }
    if whence == 1 && offset >= 0 {
        // read-and-discard emulation on the real fd.
        let mut remaining = offset;
        while remaining > 0 {
            let mut buf = vec![0u8; remaining.min(8192) as usize];
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                let e = std::io::Error::last_os_error();
                if n < 0 && e.raw_os_error() != Some(libc::EAGAIN) {
                    let (errno, msg) = io_errno_str(&e);
                    read_ebadf_notice(it, fname, errno, &msg)?;
                }
                return Ok(StdioSeek::Fail);
            }
            remaining -= n as i64;
        }
        return Ok(StdioSeek::Ok);
    }
    it.warn_pub(&format!("{}(): Stream does not support seeking", fname))?;
    Ok(StdioSeek::Fail)
}

/// php_stream_gets on a pipe: drains the read buffer and refills it
/// in chunk_size fills until '\n', `limit`, or EOF.
fn read_line_pipe(
    file: &mut std::fs::File,
    eof: &mut bool,
    nonblock: bool,
    pos: &mut u64,
    rbuf: &mut std::collections::VecDeque<u8>,
    chunk: usize,
    limit: usize,
) -> Result<StreamRead, PhpError> {
    use std::io::Read;
    let mut out = Vec::new();
    while out.len() < limit {
        if let Some(b) = rbuf.pop_front() {
            out.push(b);
            if b == b'\n' {
                break;
            }
            continue;
        }
        if *eof {
            break;
        }
        let mut buf = vec![0u8; chunk.max(1)];
        match file.read(&mut buf) {
            Ok(0) => {
                *eof = true;
            }
            Ok(got) => {
                buf.truncate(got);
                rbuf.extend(buf);
            }
            Err(e) if nonblock && e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => {
                let (errno, msg) = io_errno_str(&e);
                return Ok(StreamRead::Ebadf(errno, msg));
            }
        }
    }
    *pos += out.len() as u64;
    Ok(StreamRead::Data(out))
}

fn read_resource(it: &mut Interp, c: Option<&Cell>, n: usize) -> Result<StreamRead, PhpError> {
    match c.map(|c| c.borrow().clone()) {
        Some(Value::Resource(r)) => {
            let sv = Value::Resource(r.clone());
            let mut res = {
                let mut rb = r.borrow_mut();
                let rid = rb.id();
                // zend fails stream reads re-entered from inside a
                // php_user_filter::filter() call on the same stream.
                if it.stream_filter_busy.contains(&rid) {
                    return Ok(StreamRead::FailSilent);
                }
                // Detach the resource so user filter methods may
                // re-borrow the stream cell — they observe the
                // stand-in (meta/stat answer from preserved fields);
                // reads on the busy stream short-circuit above.
                match stream_filter_standin(&rb) {
                    Some(dummy) => std::mem::replace(&mut *rb, dummy),
                    None => return read_resource_inner(it, &mut rb, &sv, n),
                }
            };
            let out = read_resource_inner(it, &mut res, &sv, n);
            std::mem::swap(&mut *r.borrow_mut(), &mut res);
            out
        }
        _ => Err(PhpError::fatal("not a resource", 0)),
    }
}

/// read_resource's per-variant body — run on the borrowed resource
/// for stand-in-less variants, or on the detached resource while a
/// stand-in occupies the cell for user-filter re-entrancy.
fn read_resource_inner(
    it: &mut Interp,
    res: &mut PhpResource,
    sv: &Value,
    n: usize,
) -> Result<StreamRead, PhpError> {
    let rid = res.id();
    let filtered = stream_read_filtered(it, rid);
    match res {
        PhpResource::Pipe {
            file,
            pos,
            eof,
            nonblock,
            id,
            rbuf,
            ..
        } => {
            use std::os::fd::AsRawFd;
            let fd = file.as_raw_fd();
            if filtered {
                pipe_read_filtered(it, rid, sv, fd, pos, eof, rbuf, stream_chunk(it, *id), n)
            } else {
                read_pipe(file, eof, *nonblock, pos, rbuf, stream_chunk(it, *id), n)
            }
        }
        PhpResource::Stdio { which, .. } => match *which {
            // STDIN reads are not modeled; php://output has no
            // read op at all (silent false); STDOUT/STDERR are
            // write-only fds → read(2) EBADF.
            0 => Ok(StreamRead::Data(Vec::new())),
            _ if *which > 2 => Ok(StreamRead::FailSilent),
            _ => Ok(StreamRead::Ebadf(9, "Bad file descriptor".into())),
        },
        PhpResource::Input {
            body,
            pos,
            eof,
            spilled_fd,
            srbuf,
            rcap,
            fraw,
            id,
            ..
        } => {
            // After a temp_cast spill all io hits the shared fd
            // through zend's outer readbuf.
            if let Some(fd) = *spilled_fd {
                let chunk = stream_chunk(it, *id);
                if filtered {
                    fd_stream_read_filtered(it, rid, sv, fd, pos, eof, srbuf, chunk, n)
                } else {
                    Ok(fd_stream_read(fd, pos, eof, srbuf, rcap, chunk, n))
                }
            } else if filtered {
                mem_filtered_read(
                    it,
                    rid,
                    sv,
                    body,
                    fraw,
                    pos,
                    eof,
                    srbuf,
                    stream_chunk(it, *id),
                    n,
                )
            } else {
                // pos may sit past the end (fseek allows it) —
                // clamp the slice start instead of panicking.
                let start = (*pos as usize).min(body.len());
                let take = (body.len() - start).min(n);
                let out = body[start..start + take].to_vec();
                *pos += take as u64;
                *fraw = *pos;
                if take < n {
                    *eof = true;
                }
                Ok(StreamRead::Data(out))
            }
        }
        PhpResource::Mem {
            buf,
            pos,
            eof,
            spilled_fd,
            srbuf,
            rcap,
            fraw,
            id,
            ..
        } => {
            if let Some(fd) = *spilled_fd {
                let chunk = stream_chunk(it, *id);
                if filtered {
                    fd_stream_read_filtered(it, rid, sv, fd, pos, eof, srbuf, chunk, n)
                } else {
                    Ok(fd_stream_read(fd, pos, eof, srbuf, rcap, chunk, n))
                }
            } else if filtered {
                mem_filtered_read(
                    it,
                    rid,
                    sv,
                    buf,
                    fraw,
                    pos,
                    eof,
                    srbuf,
                    stream_chunk(it, *id),
                    n,
                )
            } else {
                let start = (*pos as usize).min(buf.len());
                let take = (buf.len() - start).min(n);
                let out = buf[start..start + take].to_vec();
                *pos += take as u64;
                *fraw = *pos;
                if take < n {
                    *eof = true;
                }
                Ok(StreamRead::Data(out))
            }
        }
        PhpResource::File {
            id,
            file,
            pos,
            read,
            eof,
            rbuf,
            rcap,
            ..
        } => {
            use std::os::fd::AsRawFd;
            if !*read {
                return Ok(StreamRead::Ebadf(9, "Bad file descriptor".into()));
            }
            // zend plain-file streams buffer reads — drain the
            // readbuf first, then fill greedily in chunk spans
            // (plain files are exempt from zend's single-fill
            // break). `pos` is the PHP-side ftell counter; an
            // fd dup'd to a proc_open child shares the kernel
            // offset, so a draining child moves reads to EOF.
            if filtered {
                fd_stream_read_filtered(
                    it,
                    rid,
                    sv,
                    file.as_raw_fd(),
                    pos,
                    eof,
                    rbuf,
                    stream_chunk(it, *id),
                    n,
                )
            } else {
                Ok(fd_stream_read(
                    file.as_raw_fd(),
                    pos,
                    eof,
                    rbuf,
                    rcap,
                    stream_chunk(it, *id),
                    n,
                ))
            }
        }
        _ => Ok(StreamRead::Data(Vec::new())),
    }
}

fn read_line_resource(
    it: &mut Interp,
    c: Option<&Cell>,
    limit: usize,
) -> Result<StreamRead, PhpError> {
    match c.map(|c| c.borrow().clone()) {
        Some(Value::Resource(r)) => {
            let sv = Value::Resource(r.clone());
            let mut res = {
                let mut rb = r.borrow_mut();
                let rid = rb.id();
                if it.stream_filter_busy.contains(&rid) {
                    return Ok(StreamRead::FailSilent);
                }
                match stream_filter_standin(&rb) {
                    Some(dummy) => std::mem::replace(&mut *rb, dummy),
                    None => {
                        return read_line_resource_inner(it, &mut rb, &sv, limit);
                    }
                }
            };
            let out = read_line_resource_inner(it, &mut res, &sv, limit);
            std::mem::swap(&mut *r.borrow_mut(), &mut res);
            out
        }
        _ => Err(PhpError::fatal("not a resource", 0)),
    }
}

/// read_line_resource's per-variant body — see read_resource_inner
/// for why it runs on a detached resource.
fn read_line_resource_inner(
    it: &mut Interp,
    res: &mut PhpResource,
    sv: &Value,
    limit: usize,
) -> Result<StreamRead, PhpError> {
    let rid = res.id();
    let filtered = stream_read_filtered(it, rid);
    match res {
        PhpResource::Pipe {
            file,
            pos,
            eof,
            nonblock,
            id,
            rbuf,
            ..
        } => {
            use std::os::fd::AsRawFd;
            let fd = file.as_raw_fd();
            if filtered {
                pipe_line_read_filtered(
                    it,
                    rid,
                    sv,
                    fd,
                    pos,
                    eof,
                    rbuf,
                    stream_chunk(it, *id),
                    limit,
                    true,
                )
            } else {
                read_line_pipe(
                    file,
                    eof,
                    *nonblock,
                    pos,
                    rbuf,
                    stream_chunk(it, *id),
                    limit,
                )
            }
        }
        PhpResource::Stdio { which, .. } => match *which {
            0 => Ok(StreamRead::Data(Vec::new())),
            _ if *which > 2 => Ok(StreamRead::FailSilent),
            _ => Ok(StreamRead::Ebadf(9, "Bad file descriptor".into())),
        },
        PhpResource::Input {
            body,
            pos,
            eof,
            spilled_fd,
            srbuf,
            rcap,
            fraw,
            id,
            ..
        } => {
            if let Some(fd) = *spilled_fd {
                let chunk = stream_chunk(it, *id);
                if filtered {
                    fd_line_read_filtered(it, rid, sv, fd, pos, eof, srbuf, chunk, limit)
                } else {
                    Ok(fd_line_read(fd, pos, eof, srbuf, rcap, chunk, limit))
                }
            } else if filtered {
                mem_line_read_filtered(
                    it,
                    rid,
                    sv,
                    body,
                    fraw,
                    pos,
                    eof,
                    srbuf,
                    stream_chunk(it, *id),
                    limit,
                )
            } else {
                let start = *pos as usize;
                if start >= body.len() {
                    *eof = true;
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
                    *fraw = *pos;
                    Ok(StreamRead::Data(out))
                }
            }
        }
        PhpResource::Mem {
            buf,
            pos,
            eof,
            spilled_fd,
            srbuf,
            rcap,
            fraw,
            id,
            ..
        } => {
            if let Some(fd) = *spilled_fd {
                let chunk = stream_chunk(it, *id);
                if filtered {
                    fd_line_read_filtered(it, rid, sv, fd, pos, eof, srbuf, chunk, limit)
                } else {
                    Ok(fd_line_read(fd, pos, eof, srbuf, rcap, chunk, limit))
                }
            } else if filtered {
                mem_line_read_filtered(
                    it,
                    rid,
                    sv,
                    buf,
                    fraw,
                    pos,
                    eof,
                    srbuf,
                    stream_chunk(it, *id),
                    limit,
                )
            } else {
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
                    *fraw = *pos;
                    Ok(StreamRead::Data(out))
                }
            }
        }
        PhpResource::File {
            id,
            file,
            pos,
            read,
            eof,
            rbuf,
            rcap,
            ..
        } => {
            use std::os::fd::AsRawFd;
            if !*read {
                return Ok(StreamRead::Ebadf(9, "Bad file descriptor".into()));
            }
            if filtered {
                fd_line_read_filtered(
                    it,
                    rid,
                    sv,
                    file.as_raw_fd(),
                    pos,
                    eof,
                    rbuf,
                    stream_chunk(it, *id),
                    limit,
                )
            } else {
                Ok(fd_line_read(
                    file.as_raw_fd(),
                    pos,
                    eof,
                    rbuf,
                    rcap,
                    stream_chunk(it, *id),
                    limit,
                ))
            }
        }
        _ => Ok(StreamRead::Data(Vec::new())),
    }
}

/// php_stream_gets: read up to `limit` bytes, stopping after '\n'.
/// Returns an empty vec at EOF (or on a non-readable stream).
fn csv_gets(it: &mut Interp, c: &Cell, limit: usize) -> Result<StreamRead, PhpError> {
    match c.borrow().clone() {
        Value::Resource(r) => {
            let sv = Value::Resource(r.clone());
            let mut res = {
                let mut rb = r.borrow_mut();
                let rid = rb.id();
                if it.stream_filter_busy.contains(&rid) {
                    return Ok(StreamRead::FailSilent);
                }
                match stream_filter_standin(&rb) {
                    Some(dummy) => std::mem::replace(&mut *rb, dummy),
                    None => {
                        return csv_gets_inner(it, &mut rb, &sv, limit);
                    }
                }
            };
            let out = csv_gets_inner(it, &mut res, &sv, limit);
            std::mem::swap(&mut *r.borrow_mut(), &mut res);
            out
        }
        _ => Ok(StreamRead::Data(Vec::new())),
    }
}

/// csv_gets' per-variant body — see read_resource_inner for why it
/// runs on a detached resource.
fn csv_gets_inner(
    it: &mut Interp,
    res: &mut PhpResource,
    sv: &Value,
    limit: usize,
) -> Result<StreamRead, PhpError> {
    let rid = res.id();
    let filtered = stream_read_filtered(it, rid);
    match res {
        PhpResource::Pipe {
            file,
            pos,
            eof,
            nonblock,
            id,
            rbuf,
            ..
        } => {
            use std::os::fd::AsRawFd;
            let fd = file.as_raw_fd();
            if filtered {
                pipe_line_read_filtered(
                    it,
                    rid,
                    sv,
                    fd,
                    pos,
                    eof,
                    rbuf,
                    stream_chunk(it, *id),
                    limit,
                    true,
                )
            } else {
                read_line_pipe(
                    file,
                    eof,
                    *nonblock,
                    pos,
                    rbuf,
                    stream_chunk(it, *id),
                    limit,
                )
            }
        }
        PhpResource::File {
            id,
            file,
            pos,
            read,
            eof,
            rbuf,
            rcap,
            ..
        } => {
            use std::os::fd::AsRawFd;
            if !*read {
                return Ok(StreamRead::Ebadf(9, "Bad file descriptor".into()));
            }
            if filtered {
                fd_line_read_filtered(
                    it,
                    rid,
                    sv,
                    file.as_raw_fd(),
                    pos,
                    eof,
                    rbuf,
                    stream_chunk(it, *id),
                    limit,
                )
            } else {
                Ok(fd_line_read(
                    file.as_raw_fd(),
                    pos,
                    eof,
                    rbuf,
                    rcap,
                    stream_chunk(it, *id),
                    limit,
                ))
            }
        }
        PhpResource::Mem {
            buf,
            pos,
            eof,
            spilled_fd,
            srbuf,
            rcap,
            fraw,
            id,
            ..
        } => {
            if let Some(fd) = *spilled_fd {
                let chunk = stream_chunk(it, *id);
                if filtered {
                    fd_line_read_filtered(it, rid, sv, fd, pos, eof, srbuf, chunk, limit)
                } else {
                    Ok(fd_line_read(fd, pos, eof, srbuf, rcap, chunk, limit))
                }
            } else if filtered {
                mem_line_read_filtered(
                    it,
                    rid,
                    sv,
                    buf,
                    fraw,
                    pos,
                    eof,
                    srbuf,
                    stream_chunk(it, *id),
                    limit,
                )
            } else {
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
                    *fraw = *pos;
                    Ok(StreamRead::Data(buf[start..end].to_vec()))
                }
            }
        }
        PhpResource::Input {
            body,
            pos,
            eof,
            spilled_fd,
            srbuf,
            rcap,
            fraw,
            id,
            ..
        } => {
            if let Some(fd) = *spilled_fd {
                let chunk = stream_chunk(it, *id);
                if filtered {
                    fd_line_read_filtered(it, rid, sv, fd, pos, eof, srbuf, chunk, limit)
                } else {
                    Ok(fd_line_read(fd, pos, eof, srbuf, rcap, chunk, limit))
                }
            } else if filtered {
                mem_line_read_filtered(
                    it,
                    rid,
                    sv,
                    body,
                    fraw,
                    pos,
                    eof,
                    srbuf,
                    stream_chunk(it, *id),
                    limit,
                )
            } else {
                let start = *pos as usize;
                if start >= body.len() {
                    *eof = true;
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
                    *fraw = *pos;
                    Ok(StreamRead::Data(body[start..end].to_vec()))
                }
            }
        }
        _ => Ok(StreamRead::Data(Vec::new())),
    }
}

/// php_stream_get_line equivalent: read the rest of the current line
/// (through '\n' inclusive), unbounded. Returns None at EOF.
fn csv_get_line(it: &mut Interp, c: &Cell) -> Result<StreamRead, PhpError> {
    match csv_gets(it, c, usize::MAX)? {
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
    let mut buf = match csv_gets(it, stream, limit)? {
        StreamRead::Data(b) => b,
        StreamRead::FailSilent => return Ok(Value::Bool(false)),
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
                            match csv_get_line(it, stream)? {
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
                                StreamRead::FailSilent => return Ok(Value::Bool(false)),
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
    stat_from_vals([
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
    ])
}

fn stat_from_vals(vals: [i64; 13]) -> PhpArray {
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

/// zend's synthetic statbuf for buffer-backed streams — fstat() on
/// php://memory, unspilled php://temp and RFC2397 returns this fixed
/// 26-element array: dev=12 (a virtual tmpfs-ish device), ino=0,
/// mode=0100666, nlink=1, uid=gid=0, rdev=-1, size=buffer length,
/// atime=mtime=ctime=0, blksize=blocks=-1.
fn buf_stat_array(len: usize) -> PhpArray {
    stat_from_vals([12, 0, 33206, 1, 0, 0, -1, len as i64, 0, 0, 0, -1, -1])
}

/// stream_select(&$read, &$write, &$except, ?$seconds, ?$usec): libc
/// select() over the fd of each resource in the input arrays, then the
/// arrays are rewritten to only the ready entries. NULL $seconds waits
/// forever (zend's NULL timeval).
/// zend stream_select: per-element validation runs THREE fetch passes
/// per array — the fd_set build, stream_array_emulate_read_fd_set
/// (read array only, keeps buffered streams readable), and the
/// stream_array_from_fd_set rewrite. Each bad element chains a
/// TypeError via Exception::$previous on EVERY pass (oracle: 2 bad
/// elements in the read array → depth 6); a real stream with no
/// descriptor warns its ops label on the build and emulate passes
/// only (the rewrite pass skips uncastable entries silently). With no
/// usable stream left, the ValueError is thrown ON TOP of the chain
/// and no rewrite happens; otherwise select still runs and the
/// by-ref arrays are rewritten before the chain surfaces at return.
fn stream_select(it: &mut Interp, fname: &str, args: &[Cell]) -> Result<Option<Value>, PhpError> {
    use std::os::fd::AsRawFd;
    let param_names = ["read", "write", "except"];
    // zend signature: stream_select(?array &$read, ?array &$write,
    // ?array &$except, ?int $seconds, ?int $microseconds = null) — the
    // 4th arg is REQUIRED and all five ZPP type-checks run before the
    // body: a bad $seconds TypeError surfaces BEFORE any element fetch
    // (chains, "Cannot represent" warnings, the empty-sets ValueError).
    if args.len() < 4 {
        return err(
            "ArgumentCountError",
            format!(
                "{}() expects at least 4 arguments, {} given",
                fname,
                args.len()
            ),
        );
    }
    if args.len() > 5 {
        return err(
            "ArgumentCountError",
            format!(
                "{}() expects at most 5 arguments, {} given",
                fname,
                args.len()
            ),
        );
    }
    for (ai, arg_c) in args.iter().take(3).enumerate() {
        let v = arg_c.borrow().clone();
        match v {
            Value::Array(_) | Value::Null => {}
            _ => {
                return err(
                    "TypeError",
                    format!(
                        "{}(): Argument #{} (${}) must be of type ?array, {} given",
                        fname,
                        ai + 1,
                        param_names[ai],
                        select_arg_word(&v)
                    ),
                )
            }
        }
    }
    let sec_null = matches!(*args[3].borrow(), Value::Null);
    let usec_null = args
        .get(4)
        .map(|c| matches!(*c.borrow(), Value::Null))
        .unwrap_or(true);
    let sec = if sec_null {
        0
    } else {
        zpp_long(it, args, 3, fname, 4, "$seconds", "?int")?
    };
    let usec = if usec_null {
        0
    } else {
        zpp_long(it, args, 4, fname, 5, "$microseconds", "?int")?
    };
    // zend stream_select: per-element validation runs THREE fetch passes
    // per array — the fd_set build, stream_array_emulate_read_fd_set
    // (read array only, keeps buffered streams readable), and the
    // stream_array_from_fd_set rewrite. Each bad element chains a
    // TypeError via Exception::$previous on EVERY pass (oracle: 2 bad
    // elements in the read array → depth 6); an uncastable stream warns
    // its ops label on the build and emulate passes only. With no
    // usable stream left, the ValueError is thrown ON TOP of the chain
    // and no rewrite happens; otherwise select still runs and the
    // by-ref arrays are rewritten before the chain surfaces at return.
    let mut sets: [Vec<(ArrKey, Value, i32)>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let mut chain: Vec<Value> = Vec::new();
    let mut max_fd = 0i32;
    for (ai, arg_c) in args.iter().take(3).enumerate() {
        let Value::Array(a) = &*arg_c.borrow() else {
            continue;
        };
        for (k, c) in a.borrow().iter() {
            let item = c.borrow().clone();
            let Value::Resource(ref r) = item else {
                select_chain(
                    it,
                    &mut chain,
                    "TypeError",
                    format!(
                        "{}(): supplied argument is not a valid stream resource",
                        fname
                    ),
                );
                continue;
            };
            let fd = {
                let mut rb = r.borrow_mut();
                let rid = rb.id();
                match &mut *rb {
                    // Resources that aren't streams (closed, process,
                    // ...) → zend's stream le fetch fails → TypeError.
                    PhpResource::Closed { .. }
                    | PhpResource::Proc { .. }
                    | PhpResource::Other { .. } => {
                        drop(rb);
                        select_chain(
                            it,
                            &mut chain,
                            "TypeError",
                            format!(
                                "{}(): supplied resource is not a valid stream resource",
                                fname
                            ),
                        );
                        continue;
                    }
                    // zend's cast pre-check (cast.c:300): a filtered
                    // stream fails EVERY non-STDIO cast — FOR_SELECT
                    // included — before ops->cast runs.
                    _ if stream_is_filtered(it, rid) => {
                        drop(rb);
                        it.warn_pub(&format!(
                            "{fname}(): Cannot cast a filtered stream on this system"
                        ))?;
                        continue;
                    }
                    PhpResource::File { file, .. } | PhpResource::Pipe { file, .. } => {
                        Some(file.as_raw_fd())
                    }
                    PhpResource::Stdio { which, .. } if *which <= 2 => Some(*which as i32),
                    // php://temp* and data: are fd-claimable — zend's
                    // cast spills the buffer into a tmpfile() the
                    // stream then keeps; a failed spill (or any other
                    // fd-less stream) warns its stream-type label.
                    other => match spill_fd_for_stream(other) {
                        Some(fd) => {
                            // temp_cast delegates to the inner stream's
                            // cast and drops the INTERNAL flag at that
                            // boundary — a spilled stream with buffered
                            // bytes warns "data lost" on EVERY cast,
                            // even inside select (zend cast.c:325).
                            let n = spilled_buffered(other);
                            if n > 0 {
                                it.warn_pub(&format!(
                                    "{fname}(): {n} bytes of buffered data lost during stream conversion!"
                                ))?;
                            }
                            Some(fd)
                        }
                        None => {
                            let ty = stream_ops_label(other);
                            it.warn_pub(&format!(
                                "{}(): Cannot represent a stream of type {} as a select()able descriptor",
                                fname, ty
                            ))?;
                            continue;
                        }
                    },
                }
            };
            if let Some(fd) = fd {
                sets[ai].push((k.clone(), item, fd));
                max_fd = max_fd.max(fd);
            }
        }
    }
    let sets_count = sets.iter().filter(|s| !s.is_empty()).count();
    if sets_count == 0 {
        // Zend throws the ValueError ON TOP of the collected
        // element TypeErrors and returns before select — the by-ref
        // arrays keep their original contents.
        return Err(select_throw(
            it,
            chain,
            "ValueError",
            "No stream arrays were passed",
        ));
    }
    // zend's PHP_SAFE_MAX_FD guard runs before the sec/usec checks:
    // descriptors at/over FD_SETSIZE warn and return false with the
    // arrays untouched.
    if max_fd >= libc::FD_SETSIZE as i32 {
        let fs = libc::FD_SETSIZE as i64;
        let recommended = ((max_fd as i64 + 1) + fs - 1) / fs * fs;
        it.warn_pub(&format!(
            "{}(): You MUST recompile PHP with a larger value of FD_SETSIZE.\nIt is set to {}, but you have descriptors numbered at least as high as {}.\n --enable-fd-setsize={} is recommended, but you may want to set it\nto equal the maximum number of open files supported by your system,\nin order to avoid seeing this error again at a later date.",
            fname,
            libc::FD_SETSIZE,
            max_fd,
            recommended
        ))?;
        return Ok(Some(Value::Bool(false)));
    }
    // $seconds/$microseconds semantic checks — after the fd guard,
    // before select. A non-null usec with null sec must be 0.
    // Zend folds usec into tv (sec + usec/1e6, usec%1e6) — no upper
    // bound.
    if sec_null && !usec_null && usec != 0 {
        return err(
            "ValueError",
            format!(
                "{}(): Argument #5 ($microseconds) must be null when argument #4 ($seconds) is null",
                fname
            ),
        );
    }
    if !sec_null {
        if sec < 0 {
            return err(
                "ValueError",
                format!(
                    "{}(): Argument #4 ($seconds) must be greater than or equal to 0",
                    fname
                ),
            );
        }
        if usec < 0 {
            return err(
                "ValueError",
                format!(
                    "{}(): Argument #5 ($microseconds) must be greater than or equal to 0",
                    fname
                ),
            );
        }
    }
    // zend's stream_array_emulate_read_fd_set: any read-array stream
    // whose OUTER read buffer still holds bytes counts as readable —
    // zend returns ONLY those streams instantly (w/e arrays emptied,
    // select() never called). php://temp/memory/data streams never
    // qualify: they're NO_BUFFER at the outer level, so their
    // writepos-readpos is always 0 — only file streams and proc pipes
    // buffer at this layer. Bad elements still chain a TypeError like
    // every other element pass.
    if let Some(c0) = args.first() {
        let buffered: Vec<(ArrKey, Value)> = {
            let mut buffered = Vec::new();
            if let Value::Array(a) = &*c0.borrow() {
                for (k, c) in a.borrow().iter() {
                    let item = c.borrow().clone();
                    match &item {
                        Value::Resource(r) => {
                            let rb = r.borrow();
                            match &*rb {
                                PhpResource::Closed { .. }
                                | PhpResource::Proc { .. }
                                | PhpResource::Other { .. } => select_chain(
                                    it,
                                    &mut chain,
                                    "TypeError",
                                    format!(
                                        "{}(): supplied resource is not a valid stream resource",
                                        fname
                                    ),
                                ),
                                PhpResource::File { rbuf, .. } | PhpResource::Pipe { rbuf, .. }
                                    if !rbuf.is_empty() =>
                                {
                                    buffered.push((k.clone(), item.clone()));
                                }
                                _ => {}
                            }
                        }
                        _ => select_chain(
                            it,
                            &mut chain,
                            "TypeError",
                            format!(
                                "{}(): supplied argument is not a valid stream resource",
                                fname
                            ),
                        ),
                    }
                }
            }
            buffered
        };
        if !buffered.is_empty() {
            // rewrite the read array to just the buffered streams
            // (keys preserved), empty w/e, skip select() entirely.
            let mut nr = PhpArray::new();
            for (k, v) in &buffered {
                nr.set(k.clone(), v.clone());
            }
            *c0.borrow_mut() = Value::Array(Rc::new(RefCell::new(nr)));
            for i in 1..3 {
                if let Some(c) = args.get(i) {
                    *c.borrow_mut() = Value::Array(Rc::new(RefCell::new(PhpArray::new())));
                }
            }
            if !chain.is_empty() {
                return Err(select_throw_last(it, chain));
            }
            return Ok(Some(Value::Int(buffered.len() as i64)));
        }
    }
    let mut fds = [
        unsafe { std::mem::zeroed::<libc::fd_set>() },
        unsafe { std::mem::zeroed::<libc::fd_set>() },
        unsafe { std::mem::zeroed::<libc::fd_set>() },
    ];
    for (i, s) in sets.iter().enumerate() {
        for (_, _, fd) in s {
            // PHP_SAFE_FD_SET: an fd at/over FD_SETSIZE must never be
            // FD_SET — the fd_set arrays are FD_SETSIZE bits wide, so a
            // wider descriptor would write out of bounds. (max_fd was
            // already tracked during the casts above for the guard.)
            if *fd < libc::FD_SETSIZE as i32 {
                unsafe {
                    libc::FD_SET(*fd, &mut fds[i]);
                }
            }
        }
    }
    let mut tv = libc::timeval {
        tv_sec: sec + usec / 1_000_000,
        tv_usec: usec % 1_000_000,
    };
    let n = unsafe {
        libc::select(
            max_fd + 1,
            &mut fds[0],
            &mut fds[1],
            &mut fds[2],
            if sec_null {
                std::ptr::null_mut()
            } else {
                &mut tv
            },
        )
    };
    if n < 0 {
        let e = std::io::Error::last_os_error();
        let (en, reason) = io_errno_str(&e);
        it.warn_pub(&format!(
            "stream_select(): unable to select [{}]: {} (max_fd={})",
            en, reason, max_fd
        ))?;
        return Ok(Some(Value::Bool(false)));
    }
    // Rewrite each input array to the ready entries only — zend's
    // stream_array_from_fd_set RE-CASTS every element: bad elements
    // chain a third TypeError, filtered streams and fd-less streams
    // warn AGAIN here (the emulate pass no longer pre-warns), and a
    // spilled temp's inner delegation warns "data lost" a second time.
    for (i, fd_set) in fds.iter().enumerate() {
        let mut ready = PhpArray::new();
        if let Some(c) = args.get(i) {
            if let Value::Array(a) = &*c.borrow() {
                for (k, item_c) in a.borrow().iter() {
                    let item = item_c.borrow().clone();
                    let Value::Resource(r) = &item else {
                        select_chain(
                            it,
                            &mut chain,
                            "TypeError",
                            format!(
                                "{}(): supplied argument is not a valid stream resource",
                                fname
                            ),
                        );
                        continue;
                    };
                    let fd = {
                        let mut rb = r.borrow_mut();
                        let rid = rb.id();
                        match &mut *rb {
                            PhpResource::Closed { .. }
                            | PhpResource::Proc { .. }
                            | PhpResource::Other { .. } => {
                                drop(rb);
                                select_chain(
                                    it,
                                    &mut chain,
                                    "TypeError",
                                    format!(
                                        "{}(): supplied resource is not a valid stream resource",
                                        fname
                                    ),
                                );
                                continue;
                            }
                            _ if stream_is_filtered(it, rid) => {
                                drop(rb);
                                it.warn_pub(&format!(
                                    "{fname}(): Cannot cast a filtered stream on this system"
                                ))?;
                                continue;
                            }
                            PhpResource::File { file, .. } | PhpResource::Pipe { file, .. } => {
                                use std::os::fd::AsRawFd;
                                file.as_raw_fd()
                            }
                            PhpResource::Stdio { which, .. } if *which <= 2 => *which as i32,
                            other => match spill_fd_for_stream(other) {
                                Some(fd) => {
                                    let n = spilled_buffered(other);
                                    if n > 0 {
                                        it.warn_pub(&format!(
                                            "{fname}(): {n} bytes of buffered data lost during stream conversion!"
                                        ))?;
                                    }
                                    fd
                                }
                                None => {
                                    let ty = stream_ops_label(other);
                                    it.warn_pub(&format!(
                                        "{}(): Cannot represent a stream of type {} as a select()able descriptor",
                                        fname, ty
                                    ))?;
                                    continue;
                                }
                            },
                        }
                    };
                    if unsafe { libc::FD_ISSET(fd, fd_set) } {
                        ready.set(k.clone(), item.clone());
                    }
                }
            }
            *c.borrow_mut() = Value::Array(Rc::new(RefCell::new(ready)));
        }
    }
    // A collected chain surfaces only now — select already ran and
    // the arrays were rewritten, exactly like zend propagating the
    // pending exception at function return.
    if !chain.is_empty() {
        return Err(select_throw_last(it, chain));
    }
    Ok(Some(Value::Int(n as i64)))
}

/// Append a throwable to the zend-style previous-chain and keep it as
/// the new chain tail.
fn select_chain(it: &mut Interp, chain: &mut Vec<Value>, class: &str, msg: String) {
    let e = it.exception(class, &msg);
    if let (Value::Object(o), Some(p)) = (&e, chain.last().cloned()) {
        let mut ob = o.borrow_mut();
        ob.props.insert("previous".into(), cell(p));
        if !ob.prop_order.iter().any(|k| k == "previous") {
            ob.prop_order.push("previous".into());
        }
    }
    chain.push(e);
}

/// Throw the chain tail — zend rethrows the pending exception, which
/// is the LAST error recorded (the optional ValueError `class_msg`
/// lands on top when given).
fn select_throw(it: &mut Interp, mut chain: Vec<Value>, class: &str, msg: &str) -> PhpError {
    select_chain(it, &mut chain, class, msg.to_string());
    select_throw_last(it, chain)
}

fn select_throw_last(it: &mut Interp, chain: Vec<Value>) -> PhpError {
    match chain.into_iter().last() {
        Some(v) => it.throw_value(v),
        None => PhpError::uncaught("ValueError", "No stream arrays were passed", 0),
    }
}

/// zend's php_open_temporary_fd: mkstemp on "<tmpdir>/php" + 13
/// chars of "0123456789abcdefghijklmnopqrstuv" + "XXXXXX" (the base32
/// random prefix + 6 mkstemp bytes — a 19-char random tail, matching
/// the 'uri' meta tmpfile() reports). Returns (fd, realized path);
/// the file stays LINKED — fstat nlink 1 — until the owner's dtor
/// removes it.
fn php_open_temporary_fd() -> Option<(std::os::unix::io::RawFd, String)> {
    const B32: &[u8] = b"0123456789abcdefghijklmnopqrstuv";
    let mut r: u64 = 0;
    if unsafe { libc::getrandom(&mut r as *mut u64 as *mut libc::c_void, 8, 0) } != 8 {
        r = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
            .wrapping_add(std::process::id() as u64);
    }
    let mut pfx = b"php".to_vec();
    for _ in 0..13 {
        pfx.push(B32[(r % 32) as usize]);
        r /= 32;
    }
    use std::os::unix::ffi::OsStringExt;
    let mut tmpl = std::env::temp_dir().into_os_string().into_vec();
    tmpl.push(b'/');
    tmpl.extend_from_slice(&pfx);
    tmpl.extend_from_slice(b"XXXXXX");
    tmpl.push(0);
    let fd = unsafe { libc::mkstemp(tmpl.as_mut_ptr() as *mut libc::c_char) };
    if fd < 0 {
        None
    } else {
        let name = String::from_utf8_lossy(&tmpl[..tmpl.len() - 1]).into_owned();
        Some((fd, name))
    }
}

/// php_stream_temp_cast: spill a php://temp buffer into a real
/// filesystem temp file positioned at the stream's offset. The
/// returned fd is the stream's claimable descriptor; its file is
/// removed when the last owner closes (PhpResource::Drop). None on
/// failure.
pub(in crate::builtins) fn temp_spill_fd(buf: &[u8], pos: u64) -> Option<std::os::unix::io::RawFd> {
    let (fd, name) = php_open_temporary_fd()?;
    unsafe {
        let mut off = 0usize;
        while off < buf.len() {
            let n = libc::write(fd, buf.as_ptr().add(off) as *const _, buf.len() - off);
            if n <= 0 {
                libc::close(fd);
                let _ = std::fs::remove_file(&name);
                return None;
            }
            off += n as usize;
        }
        libc::lseek(fd, pos as libc::off_t, libc::SEEK_SET);
        Some(fd)
    }
}

/// Whether `uri` is a `data:` stream (zend's RFC2397 wrapper).
fn is_data_uri(uri: &str) -> bool {
    uri.starts_with("data:")
}

/// zend's `stream->ops->label` — the stream-type name in the
/// "Cannot represent a stream of type X ..." warnings and
/// posix_isatty's "Could not use stream of type 'X'": TEMP covers
/// php://temp* (incl. maxmemory), RFC2397 covers data:.
pub(in crate::builtins) fn stream_ops_label(res: &PhpResource) -> &'static str {
    match res {
        PhpResource::Mem { temp_smax, .. } => {
            if temp_smax.is_none() {
                "MEMORY"
            } else {
                "TEMP"
            }
        }
        PhpResource::Input { uri, .. } => {
            if is_data_uri(uri) {
                "RFC2397"
            } else {
                "Input"
            }
        }
        PhpResource::Stdio { which, .. } if *which > 2 => "Output",
        PhpResource::File { .. } | PhpResource::Pipe { .. } | PhpResource::Stdio { .. } => "STDIO",
        other => other.type_name(),
    }
}

/// zend's php_stream_is_filtered — a stream with at least one attached
/// filter (either chain) fails every non-STDIO cast.
pub(in crate::builtins) fn stream_is_filtered(it: &Interp, id: u64) -> bool {
    it.stream_filters.get(&id).is_some_and(|v| !v.is_empty())
}

// -----------------------------------------------------------------
// zend stream-filter machinery (main/streams.c fill path, filter.c
// factory/attach, ext/standard/filters.c + user_filters.c).
// -----------------------------------------------------------------

/// What a factory resolution hit — the builtin state template or a
/// registered php_user_filter class name.
enum FactoryHit {
    State(Result<FilterState, String>),
    Class(String),
}

/// Which direction of the chain one run targets.
const FILTER_READ: bool = true;

/// zend's php_stream_filter_create lookup (filter.c): the full name
/// is tried first, then it is shortened a dotted segment at a time —
/// `a.b.c` → `a.b.*` → `a.*`. Every level is checked against the
/// builtin factory keys AND the user_filter_map.
/// True when `name` is an exact builtin factory name (zend:
/// php_stream_filter_register_factory_volatile collision — user
/// wildcards don't count). Reuses filter_resolve's factory arms
/// minus the user-map lookup.
fn builtin_factory(name: &str) -> Option<()> {
    // zend's factories hash keys — exact-match collision only.
    match name {
        "zlib.*" | "bzip2.*" | "convert.iconv.*" | "string.rot13" | "string.toupper"
        | "string.tolower" | "convert.*" | "consumed" | "dechunk" => Some(()),
        _ => None,
    }
}

fn filter_resolve(it: &Interp, name: &str) -> Option<FactoryHit> {
    let mut probe = name.to_string();
    loop {
        match probe.as_str() {
            "string.rot13" | "string.toupper" | "string.tolower" => {
                return Some(FactoryHit::State(Ok(FilterState::Plain)));
            }
            "consumed" => {
                return Some(FactoryHit::State(Ok(FilterState::Consumed {
                    count: 0,
                    offset: None,
                })));
            }
            "dechunk" => {
                return Some(FactoryHit::State(Ok(FilterState::Dechunk(Dechunk {
                    chunk_size: 0,
                    state: DechunkState::SizeStart,
                }))));
            }
            "convert.iconv.*" => {
                let spec = &name["convert.iconv.".len()..];
                return Some(match parse_iconv_spec(spec) {
                    Some(state) => FactoryHit::State(Ok(state)),
                    None => FactoryHit::State(Err(String::new())),
                });
            }
            "convert.*" => {
                let member = &name[name.find('.').map(|i| i + 1).unwrap_or(0)..];
                let st = match member.to_ascii_lowercase().as_str() {
                    "base64-encode" => FilterState::Base64 {
                        decode: false,
                        tail: Vec::new(),
                    },
                    "base64-decode" => FilterState::Base64 {
                        decode: true,
                        tail: Vec::new(),
                    },
                    "quoted-printable-encode" => FilterState::Qp {
                        encode: true,
                        col: 0,
                        tail: Vec::new(),
                    },
                    "quoted-printable-decode" => FilterState::Qp {
                        encode: false,
                        col: 0,
                        tail: Vec::new(),
                    },
                    _ => return Some(FactoryHit::State(Err(String::new()))),
                };
                return Some(FactoryHit::State(Ok(st)));
            }
            "zlib.*" | "bzip2.*" => {
                let member = &name[name.find('.').map(|i| i + 1).unwrap_or(0)..];
                let kind = if probe.starts_with("zlib") {
                    match member {
                        "deflate" => Some(CodecKind::ZlibDeflate),
                        "inflate" => Some(CodecKind::ZlibInflate),
                        _ => None,
                    }
                } else {
                    match member {
                        "compress" => Some(CodecKind::BzDeflate),
                        "decompress" => Some(CodecKind::BzInflate),
                        _ => None,
                    }
                };
                return Some(FactoryHit::State(match kind {
                    Some(k) => Ok(FilterState::Codec(k)),
                    None => Err(String::new()),
                }));
            }
            _ => {}
        }
        for (n, cls) in &it.user_filter_map {
            if probe == *n {
                return Some(FactoryHit::Class(cls.clone()));
            }
        }
        let i = probe.rfind('.')?;
        if probe.ends_with(".*") {
            // 'a.b.*' missed: drop '.b.*' and try 'a.*' next
            // (zend's strrchr walk pops one real segment).
            let stem = &probe[..probe.len() - 2];
            probe = format!("{}.*", &stem[..stem.rfind('.')?]);
        } else {
            probe.truncate(i + 1);
            probe.push('*');
        }
    }
}

/// 'php://filter/...spec.../resource=inner' → (spec segments,
/// inner URI). zend (php_stream_url_wrap_php) locates '/resource='
/// with strstr: the inner URI is the verbatim tail after it, and the
/// spec is every '/'-segment before it. inner=None mirrors zend's
/// 'No URL resource specified' error. When '/resource=' sits at the
/// very front (bare 'php://filter/resource=/tmp/x'), zend's strtok
/// runs over the resource text itself — every '/'-piece gets
/// filter-attempted (that is why the pieces warn).
fn php_filter_uri(path: &str) -> Option<(Vec<String>, Option<String>)> {
    let rest = path.strip_prefix("php://filter")?;
    if !rest.starts_with('/') {
        return None;
    }
    let rp = match rest.find("/resource=") {
        Some(rp) => rp,
        None => return Some((Vec::new(), None)),
    };
    let inner = Some(rest[rp + 10..].to_string());
    let segs: Vec<String> = if rp == 0 {
        rest[1..]
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    } else {
        rest[1..rp]
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    };
    Some((segs, inner))
}

/// Attach one named filter to a chain without a user-visible filter
/// resource (zend's php://filter attachments are anonymous — there's
/// no way to stream_filter_remove() them). Mirrors
/// stream_filter_attach minus the returned resource.
fn attach_anon_filter(
    it: &mut Interp,
    stream_val: &Value,
    name: &str,
    read_dir: bool,
) -> Result<(), PhpError> {
    let Value::Resource(r) = stream_val else {
        return Ok(());
    };
    let sid = r.borrow().id();
    match filter_instantiate(it, stream_val, name, None)? {
        Some(mut entry) => {
            entry.fid = it.next_res_id();
            entry.read = read_dir;
            if let FilterState::Codec(kind) = entry.state {
                it.codec_states
                    .insert((entry.fid, read_dir), CodecState::new(kind));
            }
            entry.write = !read_dir;
            it.stream_filters.entry(sid).or_default().push(entry);
        }
        None => {
            it.warn_pub(&format!(
                "{}(): Unable to create filter ({name})",
                it.filter_warn_ctx
            ))?;
        }
    }
    Ok(())
}

/// The php://filter wrapper's stream-open: open the inner resource
/// through the same fopen arm, then attach the named read/write
/// filters (zend: php_filter_url_stream_open).
fn open_filter_resource(
    it: &mut Interp,
    segs: &[String],
    inner: &str,
    mode: &str,
    ctx: &str,
    full_path: &str,
) -> Result<Option<Value>, PhpError> {
    // zend's mode_rw: a segment with no read=/write= prefix lands on
    // the chains the open MODE enables ('r'|'+' → read, 'w'|'+'|'a' →
    // write).
    let read_ok = mode.contains('r') || mode.contains('+');
    let write_ok = mode.contains('w') || mode.contains('+') || mode.contains('a');
    let mut reads: Vec<String> = Vec::new();
    let mut writes: Vec<String> = Vec::new();
    for seg in segs {
        if seg.len() >= 5 && seg[..5].eq_ignore_ascii_case("read=") {
            reads.extend(
                seg[5..]
                    .split('|')
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            );
        } else if seg.len() >= 6 && seg[..6].eq_ignore_ascii_case("write=") {
            writes.extend(
                seg[6..]
                    .split('|')
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            );
        } else {
            if read_ok {
                reads.extend(seg.split('|').filter(|s| !s.is_empty()).map(str::to_string));
            }
            if write_ok {
                writes.extend(seg.split('|').filter(|s| !s.is_empty()).map(str::to_string));
            }
        }
    }
    let prev = std::mem::replace(&mut it.filter_warn_ctx, ctx.to_string());
    // zend runs the resource= inner open with REPORT_ERRORS off —
    // a failure surfaces once, at the wrapper, as 'operation failed'.
    let inner_args = [
        cell(Value::str(inner.to_string())),
        cell(Value::str(mode.to_string())),
    ];
    let opened = it.silenced_pub(|it| dispatch(it, "fopen", &inner_args))?;
    // inner fopen reset the warn prefix to itself — restore the
    // caller's context before the anonymous attaches run.
    it.filter_warn_ctx = ctx.to_string();
    let out = match opened {
        Some(v @ Value::Resource(_)) => {
            for f in &reads {
                attach_anon_filter(it, &v, f, true)?;
            }
            for f in &writes {
                attach_anon_filter(it, &v, f, false)?;
            }
            Some(v)
        }
        _ => {
            it.warn_pub(&format!(
                "{ctx}({full_path}): Failed to open stream: operation failed"
            ))?;
            None
        }
    };
    it.filter_warn_ctx = prev;
    Ok(out)
}

/// 'convert.iconv.FROM.TO' or '.../FROM/TO' — split the spec on the
/// first '/' or '.', strip '//FLAG' suffixes, normalize both
/// encoding names, and validate against the encodings this build
/// recognizes (zend hands the pair to iconv_open). An empty side
/// means passthrough — zend attaches those specs and the bytes go
/// through untouched.
fn parse_iconv_spec(spec: &str) -> Option<FilterState> {
    let cut = spec.find(['/', '.'])?;
    let raw_from = &spec[..cut];
    let raw_to = &spec[cut + 1..];
    if raw_from.is_empty() || raw_to.is_empty() {
        // 'convert.iconv..UTF-8' / 'convert.iconv.UTF-8.' — attach OK,
        // no conversion in either direction.
        return Some(FilterState::Plain);
    }
    let (from, _) = iconv_split_flags(raw_from);
    let (to, to_flags) = iconv_split_flags(raw_to);
    let from = iconv_enc_normal(from);
    let to = iconv_enc_normal(to);
    if iconv_enc_known(&from) && iconv_enc_known(&to) {
        Some(FilterState::Iconv {
            from,
            to,
            disp: format!("\"{}\"=>\"{}\"", raw_from, raw_to),
            pending: Vec::new(),
            bom_done: false,
            translit: to_flags.iter().any(|f| f.eq_ignore_ascii_case("TRANSLIT")),
        })
    } else {
        None
    }
}

/// Split 'UTF-8//IGNORE//TRANSLIT'-style suffixes off an iconv spec
/// side — returns (bare charset, flag names).
fn iconv_split_flags(side: &str) -> (&str, Vec<&str>) {
    match side.find("//") {
        Some(i) => (&side[..i], side[i + 2..].split("//").collect()),
        None => (side, Vec::new()),
    }
}

/// Uppercase, strip '-'/'_' — "ISO-8859-1" → "ISO88591".
fn iconv_enc_normal(enc: &str) -> String {
    enc.chars()
        .filter(|c| *c != '-' && *c != '_')
        .flat_map(|c| c.to_uppercase())
        .collect()
}

/// Encodings the attach-time check accepts — the common iconv set.
fn iconv_enc_known(enc: &str) -> bool {
    if enc.is_empty() {
        return false;
    }
    // patterned families: CP1252 / WINDOWS1251 / ISO8859X / IBMx / EBCDIC-x
    if let Some(rest) = enc
        .strip_prefix("ISO8859")
        .or_else(|| enc.strip_prefix("CP"))
        .or_else(|| enc.strip_prefix("WINDOWS"))
        .or_else(|| enc.strip_prefix("IBM"))
    {
        return !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit());
    }
    if enc.starts_with("EBCDIC") {
        return true;
    }
    matches!(
        enc,
        "UTF8"
            | "UTF16"
            | "UTF16LE"
            | "UTF16BE"
            | "UTF32"
            | "UTF32LE"
            | "UTF32BE"
            | "UCS2"
            | "UCS2LE"
            | "UCS2BE"
            | "UCS4"
            | "UCS4LE"
            | "UCS4BE"
            | "UCS4INTERNAL"
            | "UCS2INTERNAL"
            | "UCS2LEINTERNAL"
            | "UCS2BEINTERNAL"
            | "UTF7"
            | "UTF7IMAP"
            | "ASCII"
            | "USASCII"
            | "ANSIX341968"
            | "LATIN1"
            | "LATIN2"
            | "LATIN3"
            | "LATIN4"
            | "LATIN5"
            | "LATIN6"
            | "LATIN7"
            | "LATIN8"
            | "LATIN9"
            | "LATIN10"
            | "KOI8R"
            | "KOI8U"
            | "KOI8"
            | "KOI8T"
            | "KOI8RU"
            | "ARMSCII8"
            | "TIS620"
            | "VISCII"
            | "EUCJP"
            | "EUCCN"
            | "EUCKR"
            | "EUCTW"
            | "EUCJISX0213"
            | "SJIS"
            | "SHIFTJIS"
            | "SHIFTJISX0213"
            | "ISO2022JP"
            | "GB2312"
            | "GBK"
            | "GB18030"
            | "BIG5"
            | "BIG5HKSCS"
            | "MACINTOSH"
            | "MACROMAN"
            | "HPROMAN8"
            | "T64"
            | "T65"
            | "TCVN"
            | "GEORGIANACADEMY"
            | "GEORGIANPS"
            | "TURKISH8"
            | "MULELAO1"
            | "NEXTSTEP"
            | "RISCOSLATIN1"
            | "PT154"
            | "RK1048"
    )
}

/// Decode bytes into code points. Returns (code points, bytes used,
/// saw-an-invalid-sequence). Unrecognized-in-practice encodings
/// decode byte-wise as ISO-8859-1.
fn iconv_decode(enc: &str, bytes: &[u8]) -> (Vec<u32>, usize, bool) {
    match enc {
        "UTF8" => iconv_decode_utf8(bytes),
        "UTF16LE" => iconv_decode_u16(bytes, true, false),
        "UTF16BE" => iconv_decode_u16(bytes, false, false),
        "UCS2" | "UCS2INTERNAL" | "UCS2LE" | "UCS2LEINTERNAL" => {
            iconv_decode_u16(bytes, true, true)
        }
        "UCS2BE" | "UCS2BEINTERNAL" => iconv_decode_u16(bytes, false, true),
        "UTF32LE" | "UCS4LE" | "UCS4" | "UCS4INTERNAL" => iconv_decode_u32(bytes, true),
        "UTF32BE" | "UCS4BE" => iconv_decode_u32(bytes, false),
        "UTF16" => iconv_decode_u16_bom(bytes),
        "UTF32" => iconv_decode_u32_bom(bytes),
        "SJIS" | "SHIFTJIS" | "SHIFTJISX0213" | "EUCCN" | "GB2312" | "EUCKR" | "EUCJP"
        | "EUCJISX0213" | "EUCTW" | "GBK" | "BIG5" | "BIG5HKSCS" => {
            iconv_decode_structural(enc, bytes)
        }
        _ => (
            bytes.iter().map(|b| *b as u32).collect(),
            bytes.len(),
            false,
        ),
    }
}

/// 'UTF-16' without LE/BE: BOM-tolerant; absent a BOM glibc picks
/// the host order — little-endian on the platforms phpun targets.
fn iconv_decode_u16_bom(bytes: &[u8]) -> (Vec<u32>, usize, bool) {
    if bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] == 0xFE {
        let (cps, used, invalid) = iconv_decode_u16(&bytes[2..], true, false);
        return (cps, used + 2.min(bytes.len()), invalid);
    }
    if bytes.len() >= 2 && bytes[0] == 0xFE && bytes[1] == 0xFF {
        let (cps, used, invalid) = iconv_decode_u16(&bytes[2..], false, false);
        return (cps, used + 2.min(bytes.len()), invalid);
    }
    iconv_decode_u16(bytes, true, false)
}

fn iconv_decode_u32_bom(bytes: &[u8]) -> (Vec<u32>, usize, bool) {
    if bytes.len() >= 4 && bytes[..4] == [0xFF, 0xFE, 0, 0] {
        let (cps, used, invalid) = iconv_decode_u32(&bytes[4..], true);
        return (cps, used + 4.min(bytes.len()), invalid);
    }
    if bytes.len() >= 4 && bytes[..4] == [0, 0, 0xFE, 0xFF] {
        let (cps, used, invalid) = iconv_decode_u32(&bytes[4..], false);
        return (cps, used + 4.min(bytes.len()), invalid);
    }
    iconv_decode_u32(bytes, true)
}

/// UTF-8 → code points. An incomplete tail is left undecoded
/// (used < len) so it can carry into the next call; an invalid
/// sequence flags the call — zend's iconv filter warns and
/// discards the brigade on EILSEQ.
fn iconv_decode_utf8(bytes: &[u8]) -> (Vec<u32>, usize, bool) {
    let mut cps = Vec::new();
    let mut i = 0;
    let mut invalid = false;
    while i < bytes.len() {
        let b = bytes[i];
        let (len, min): (usize, u32) = match b {
            0x00..=0x7F => (1, 0),
            0xC0..=0xDF => (2, 0x80),
            0xE0..=0xEF => (3, 0x800),
            // Only F0-F4 are valid 4-byte leads, but F5-F7 flag via
            // the cp > 0x10FFFF check once their continuation lands;
            // at a buffer end they still park as "incomplete" like
            // zend does — the EILSEQ surfaces on the next touch.
            0xF0..=0xF7 => (4, 0x10000),
            // Stray continuations (80-BF) and never-leads (F8-FF)
            // are EILSEQ at any position — not an incomplete tail.
            _ => {
                cps.push(0xFFFD);
                invalid = true;
                i += 1;
                continue;
            }
        };
        if i + len > bytes.len() {
            break;
        }
        let lead_mask = [0x7F, 0x1F, 0x0F, 0x07][len - 1];
        let mut cp = (b as u32) & lead_mask;
        let mut valid = true;
        for j in 1..len {
            let c = bytes[i + j];
            if !(0x80..=0xBF).contains(&c) {
                valid = false;
                break;
            }
            cp = (cp << 6) | (c as u32 & 0x3F);
        }
        if !valid || cp < min || cp > 0x10FFFF || (0xD800..=0xDFFF).contains(&cp) {
            cps.push(0xFFFD);
            invalid = true;
            i += 1;
            continue;
        }
        cps.push(cp);
        i += len;
    }
    (cps, i, invalid)
}

/// `ucs2` marks the UCS-2 family: surrogate code units are illegal
/// there (UCS-2 has no pairing), so D800-DFFF flags EILSEQ wherever
/// it lands — including at a buffer end. UTF-16 instead parks a
/// leading high surrogate waiting for its low half.
fn iconv_decode_u16(bytes: &[u8], le: bool, ucs2: bool) -> (Vec<u32>, usize, bool) {
    let mut cps = Vec::new();
    let mut i = 0;
    let mut invalid = false;
    while i + 2 <= bytes.len() {
        let u = if le {
            u16::from_le_bytes([bytes[i], bytes[i + 1]])
        } else {
            u16::from_be_bytes([bytes[i], bytes[i + 1]])
        };
        if ucs2 && (0xD800..0xE000).contains(&u) {
            cps.push(0xFFFD);
            invalid = true;
            i += 2;
            continue;
        }
        if (0xD800..0xDC00).contains(&u) {
            // High surrogate: park the whole pair when the low half
            // hasn't arrived; a non-low follower is EILSEQ.
            if i + 4 > bytes.len() {
                break;
            }
            let u2 = if le {
                u16::from_le_bytes([bytes[i + 2], bytes[i + 3]])
            } else {
                u16::from_be_bytes([bytes[i + 2], bytes[i + 3]])
            };
            if (0xDC00..0xE000).contains(&u2) {
                cps.push(0x10000 + (((u as u32 - 0xD800) << 10) | (u2 as u32 - 0xDC00)));
                i += 4;
                continue;
            }
            cps.push(0xFFFD);
            invalid = true;
            i += 2;
            continue;
        }
        if (0xDC00..0xE000).contains(&u) {
            // Lone low surrogate — EILSEQ.
            cps.push(0xFFFD);
            invalid = true;
            i += 2;
            continue;
        }
        cps.push(u as u32);
        i += 2;
    }
    (cps, i, invalid)
}

/// Structural validity for the CJK multibyte encodings we carry no
/// mapping tables for: walks each encoding's lead/trail byte rules
/// so malformed input flags EILSEQ like glibc. Valid sequences
/// still decode byte-wise (their code-point mapping is a known
/// divergence); an incomplete tail stays pending for the next call.
fn iconv_decode_structural(enc: &str, bytes: &[u8]) -> (Vec<u32>, usize, bool) {
    let mut i = 0;
    let mut invalid = false;
    while i < bytes.len() {
        let b = bytes[i];
        let len = match enc {
            "SJIS" | "SHIFTJIS" | "SHIFTJISX0213" => match b {
                0x00..=0x7F | 0xA1..=0xDF => 1,
                0x81..=0x9F | 0xE0..=0xFC => 2,
                _ => 0,
            },
            "EUCCN" | "GB2312" => match b {
                0x00..=0x7F => 1,
                0xA1..=0xFE => 2,
                _ => 0,
            },
            "GBK" => match b {
                0x00..=0x7F => 1,
                0x81..=0xFE => 2,
                _ => 0,
            },
            "EUCKR" => match b {
                0x00..=0x7F => 1,
                0x81..=0xFE => 2,
                _ => 0,
            },
            "EUCJP" | "EUCJISX0213" => match b {
                0x00..=0x7F => 1,
                0x8E => 2,
                0x8F => 3,
                0xA1..=0xFE => 2,
                _ => 0,
            },
            "EUCTW" => match b {
                0x00..=0x7F => 1,
                0x8E => 4,
                0xA1..=0xFE => 2,
                _ => 0,
            },
            "BIG5" | "BIG5HKSCS" => match b {
                0x00..=0x7F => 1,
                0x81..=0xFE => 2,
                _ => 0,
            },
            _ => 1,
        };
        if len == 0 {
            invalid = true;
            i += 1;
            continue;
        }
        if i + len > bytes.len() {
            break;
        }
        let trail_ok = match (enc, len) {
            ("SJIS" | "SHIFTJIS" | "SHIFTJISX0213", 2) => {
                matches!(bytes[i + 1], 0x40..=0x7E | 0x80..=0xFC)
            }
            ("EUCCN" | "GB2312", 2) => matches!(bytes[i + 1], 0xA1..=0xFE),
            ("GBK", 2) => matches!(bytes[i + 1], 0x40..=0xFE) && bytes[i + 1] != 0x7F,
            ("EUCKR", 2) => matches!(bytes[i + 1], 0x41..=0x5A | 0x61..=0x7A | 0x81..=0xFE),
            ("EUCJP" | "EUCJISX0213", 2) if b == 0x8E => matches!(bytes[i + 1], 0xA1..=0xDF),
            ("EUCJP" | "EUCJISX0213", 2) => matches!(bytes[i + 1], 0xA1..=0xFE),
            ("EUCJP" | "EUCJISX0213", 3) => {
                matches!(bytes[i + 1], 0xA1..=0xFE) && matches!(bytes[i + 2], 0xA1..=0xFE)
            }
            ("EUCTW", 2) => matches!(bytes[i + 1], 0xA1..=0xFE),
            ("EUCTW", 4) => {
                matches!(bytes[i + 1], 0xA1..=0xB0)
                    && matches!(bytes[i + 2], 0xA1..=0xFE)
                    && matches!(bytes[i + 3], 0xA1..=0xFE)
            }
            ("BIG5" | "BIG5HKSCS", 2) => matches!(bytes[i + 1], 0x40..=0x7E | 0xA1..=0xFE),
            _ => true,
        };
        if !trail_ok {
            invalid = true;
            i += 1;
            continue;
        }
        i += len;
    }
    let cps: Vec<u32> = bytes[..i].iter().map(|b| *b as u32).collect();
    (cps, i, invalid)
}

fn iconv_decode_u32(bytes: &[u8], le: bool) -> (Vec<u32>, usize, bool) {
    let mut cps = Vec::new();
    let mut i = 0;
    let mut invalid = false;
    while i + 4 <= bytes.len() {
        let w = &bytes[i..i + 4];
        let u = if le {
            u32::from_le_bytes([w[0], w[1], w[2], w[3]])
        } else {
            u32::from_be_bytes([w[0], w[1], w[2], w[3]])
        };
        if u > 0x10FFFF || (0xD800..=0xDFFF).contains(&u) {
            cps.push(0xFFFD);
            invalid = true;
        } else {
            cps.push(u);
        }
        i += 4;
    }
    (cps, i, invalid)
}

/// Encode code points back to bytes in the target encoding. `bom`
/// gates the UTF-16/32 byte-order mark (iconv emits it on the first
/// call only); `translit` is the to-charset's //TRANSLIT flag —
/// unrepresentable code points transliterate (or '?'-fallback)
/// instead of EILSEQ-failing the call. Returns None on EILSEQ:
/// an unrepresentable code point in a non-translit target.
fn iconv_encode(enc: &str, cps: &[u32], bom: bool, translit: bool) -> Option<Vec<u8>> {
    match enc {
        "UTF8" => {
            let mut out = Vec::new();
            for &cp in cps {
                let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
                let mut b = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
            }
            Some(out)
        }
        "UTF16" | "UTF16LE" | "UCS2LE" | "UCS2LEINTERNAL" => {
            Some(iconv_encode_u16(cps, true, bom && enc == "UTF16"))
        }
        "UTF16BE" | "UCS2" | "UCS2BE" | "UCS2INTERNAL" | "UCS2BEINTERNAL" => {
            Some(iconv_encode_u16(cps, false, false))
        }
        "UTF32" | "UTF32LE" | "UCS4LE" | "UCS4INTERNAL" => {
            Some(iconv_encode_u32(cps, true, bom && enc == "UTF32"))
        }
        "UTF32BE" | "UCS4BE" | "UCS4" => Some(iconv_encode_u32(cps, false, false)),
        "ASCII" | "USASCII" | "ANSIX341968" => iconv_encode_narrow(cps, 0x80, translit),
        _ => iconv_encode_narrow(cps, 0x100, translit),
    }
}

/// Single-byte target charset — cps beyond `limit` transliterate
/// under //TRANSLIT (unknown → '?') or fail the whole call (None).
fn iconv_encode_narrow(cps: &[u32], limit: u32, translit: bool) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(cps.len());
    for &cp in cps {
        if cp < limit {
            out.push(cp as u8);
        } else if translit {
            out.extend_from_slice(iconv_translit(cp).unwrap_or("?").as_bytes());
        } else {
            return None;
        }
    }
    Some(out)
}

/// glibc iconv's //TRANSLIT approximations for the common cases;
/// unmapped code points fall back to '?' in the caller.
fn iconv_translit(cp: u32) -> Option<&'static str> {
    Some(match cp {
        0x00C0..=0x00C5 => "A",
        0x00C6 => "AE",
        0x00C7 => "C",
        0x00C8..=0x00CB => "E",
        0x00CC..=0x00CF => "I",
        0x00D0 => "D",
        0x00D1 => "N",
        0x00D2..=0x00D6 | 0x00D8 => "O",
        0x00D9..=0x00DC => "U",
        0x00DD | 0x0178 => "Y",
        0x00DE => "TH",
        0x00DF => "ss",
        0x00E0..=0x00E5 => "a",
        0x00E6 => "ae",
        0x00E7 => "c",
        0x00E8..=0x00EB => "e",
        0x00EC..=0x00EF => "i",
        0x00F0 => "d",
        0x00F1 => "n",
        0x00F2..=0x00F6 | 0x00F8 => "o",
        0x00F9..=0x00FC => "u",
        0x00FD | 0x00FF => "y",
        0x00FE => "th",
        0x0152 => "OE",
        0x0153 => "oe",
        0x00A9 => "(C)",
        0x00AE => "(R)",
        0x20AC => "EUR",
        _ => return None,
    })
}

fn iconv_encode_u16(cps: &[u32], le: bool, bom: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if bom {
        out.extend_from_slice(if le { &[0xFF, 0xFE] } else { &[0xFE, 0xFF] });
    }
    for &cp in cps {
        if cp > 0xFFFF {
            let v = cp - 0x10000;
            for u in [0xD800 + (v >> 10) as u16, 0xDC00 + (v & 0x3FF) as u16] {
                let b = if le { u.to_le_bytes() } else { u.to_be_bytes() };
                out.extend_from_slice(&b);
            }
        } else {
            let u = cp as u16;
            let b = if le { u.to_le_bytes() } else { u.to_be_bytes() };
            out.extend_from_slice(&b);
        }
    }
    out
}

fn iconv_encode_u32(cps: &[u32], le: bool, bom: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if bom {
        let b: &[u8] = if le {
            &[0xFF, 0xFE, 0, 0]
        } else {
            &[0, 0, 0xFE, 0xFF]
        };
        out.extend_from_slice(b);
    }
    for &cp in cps {
        let b = if le {
            cp.to_le_bytes()
        } else {
            cp.to_be_bytes()
        };
        out.extend_from_slice(&b);
    }
    out
}

/// zend's php_dechunk (filters.c) — the HTTP chunked-transfer decoder
/// state machine operating in place; returns the decoded length.
fn php_dechunk(d: &mut crate::value::Dechunk, buf: &mut Vec<u8>) {
    use crate::value::DechunkState as S;
    let end = buf.len();
    let mut p = 0;
    let mut out: Vec<u8> = Vec::with_capacity(end);
    while p < end {
        match d.state {
            S::SizeStart => {
                d.chunk_size = 0;
                d.state = S::Size;
            }
            S::Size => {
                while p < end {
                    let c = buf[p];
                    if c.is_ascii_hexdigit() {
                        d.chunk_size = d.chunk_size * 16 + (c as char).to_digit(16).unwrap() as u64;
                        d.state = S::Size;
                        p += 1;
                    } else if d.state == S::SizeStart {
                        d.state = S::Error;
                        break;
                    } else {
                        d.state = S::SizeExt;
                        break;
                    }
                }
                if d.state == S::Error {
                    continue;
                } else if p == end {
                    *buf = out;
                    return;
                }
            }
            S::SizeExt => {
                while p < end && buf[p] != b'\r' && buf[p] != b'\n' {
                    p += 1;
                }
                if p == end {
                    *buf = out;
                    return;
                }
                d.state = S::SizeCr;
            }
            S::SizeCr => {
                if buf[p] == b'\r' {
                    p += 1;
                    if p == end {
                        d.state = S::SizeLf;
                        *buf = out;
                        return;
                    }
                }
                d.state = S::SizeLf;
            }
            S::SizeLf => {
                if buf[p] == b'\n' {
                    p += 1;
                    if d.chunk_size == 0 {
                        d.state = S::Trailer;
                        continue;
                    } else if p == end {
                        d.state = S::Body;
                        *buf = out;
                        return;
                    }
                } else {
                    d.state = S::Error;
                    continue;
                }
                d.state = S::Body;
            }
            S::Body => {
                if (end - p) as u64 >= d.chunk_size {
                    out.extend_from_slice(&buf[p..p + d.chunk_size as usize]);
                    p += d.chunk_size as usize;
                    if p == end {
                        d.state = S::BodyCr;
                        *buf = out;
                        return;
                    }
                } else {
                    out.extend_from_slice(&buf[p..end]);
                    d.chunk_size -= (end - p) as u64;
                    d.state = S::Body;
                    *buf = out;
                    return;
                }
                d.state = S::BodyCr;
            }
            S::BodyCr => {
                if buf[p] == b'\r' {
                    p += 1;
                    if p == end {
                        d.state = S::BodyLf;
                        *buf = out;
                        return;
                    }
                }
                d.state = S::BodyLf;
            }
            S::BodyLf => {
                if buf[p] == b'\n' {
                    p += 1;
                    d.state = S::SizeStart;
                    continue;
                } else {
                    d.state = S::Error;
                    continue;
                }
            }
            S::Trailer => {
                p = end;
                continue;
            }
            S::Error => {
                out.extend_from_slice(&buf[p..end]);
                *buf = out;
                return;
            }
        }
    }
    *buf = out;
}

/// base64 helpers (zend php_conv_base64_*): encode keeps a <3-byte
/// tail; decode skips non-alphabet bytes and keeps a <4-char tail.
fn b64_encode_filter(tail: &mut Vec<u8>, input: &[u8], closing: bool) -> Vec<u8> {
    tail.extend_from_slice(input);
    let whole = tail.len() / 3 * 3;
    let mut out = super::crypto::base64_encode(&tail[..whole]).into_bytes();
    tail.drain(..whole);
    if closing && !tail.is_empty() {
        out.extend_from_slice(super::crypto::base64_encode(tail).as_bytes());
        tail.clear();
    }
    out
}

fn b64_decode_filter(tail: &mut Vec<u8>, input: &[u8], closing: bool) -> Vec<u8> {
    for &b in input {
        if b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=' {
            tail.push(b);
        }
    }
    let whole = tail.len() / 4 * 4;
    let mut out = Vec::new();
    let mut used = 0usize;
    if whole > 0 {
        match base64_decode(std::str::from_utf8(&tail[..whole]).unwrap_or("")) {
            Some(v) => {
                out = v;
                used = whole;
            }
            None => {
                // a quad containing '=' stops decoding; keep the
                // remainder as tail.
                used = whole;
            }
        }
    }
    tail.drain(..used);
    if closing {
        if let Some(v) = base64_decode(std::str::from_utf8(tail).unwrap_or("")) {
            out.extend(v);
        }
        tail.clear();
    }
    out
}

/// quoted-printable codec (zend php_conv_qprint_*): encode wraps at
/// 75 columns with '=\r\n'; decode turns '=XX' into bytes and drops
/// soft line breaks.
fn qp_encode_filter(col: &mut usize, input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let emit = |out: &mut Vec<u8>, col: &mut usize, s: &[u8]| {
        if *col + s.len() > 75 {
            out.extend_from_slice(b"=\r\n");
            *col = 0;
        }
        out.extend_from_slice(s);
        *col += s.len();
    };
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        match b {
            b'=' => emit(&mut out, col, b"=3D"),
            b'\t' | b' ' => {
                // at line end these must be escaped; lookahead for EOL
                let eol = matches!(input.get(i + 1), Some(b'\r') | Some(b'\n'));
                if eol {
                    emit(&mut out, col, &[b'=', b'0' + (b >> 4), hex_digit(b & 0xF)]);
                } else {
                    emit(&mut out, col, &[b]);
                }
            }
            b'\r' if input.get(i + 1) == Some(&b'\n') => {
                out.extend_from_slice(b"\r\n");
                *col = 0;
                i += 1;
            }
            b'\n' => {
                out.push(b'\n');
                *col = 0;
            }
            33..=60 | 62..=126 => emit(&mut out, col, &[b]),
            _ => emit(
                &mut out,
                col,
                &[b'=', hex_digit(b >> 4), hex_digit(b & 0xF)],
            ),
        }
        i += 1;
    }
    out
}

fn hex_digit(v: u8) -> u8 {
    b"0123456789ABCDEF"[v as usize & 0xF]
}

fn qp_decode_filter(tail: &mut Vec<u8>, input: &[u8], closing: bool) -> Vec<u8> {
    tail.extend_from_slice(input);
    let mut out = Vec::new();
    let mut i = 0;
    while i < tail.len() {
        if tail[i] == b'=' {
            if i + 1 >= tail.len() {
                break; // lone '=' — wait for more input
            }
            let c1 = tail[i + 1];
            if c1 == b'\r' {
                if i + 2 >= tail.len() {
                    break;
                }
                if tail[i + 2] == b'\n' {
                    i += 3;
                    continue;
                }
                i += 1;
                continue;
            }
            if c1 == b'\n' {
                i += 2;
                continue;
            }
            if i + 2 >= tail.len() {
                break;
            }
            let c2 = tail[i + 2];
            if c1.is_ascii_hexdigit() && c2.is_ascii_hexdigit() {
                out.push(
                    (c1 as char).to_digit(16).unwrap() as u8 * 16
                        + (c2 as char).to_digit(16).unwrap() as u8,
                );
                i += 3;
                continue;
            }
            // invalid escape — zend emits the '=' literally
            out.push(b'=');
            i += 1;
            continue;
        }
        out.push(tail[i]);
        i += 1;
    }
    let keep = tail[i..].to_vec();
    *tail = keep;
    if closing {
        out.extend_from_slice(tail);
        tail.clear();
    }
    out
}

/// The user-filter call — zend's userfilter_filter: ->stream is set
/// to the stream resource for the duration of filter(), the
/// brigades are registered as resources, $consumed is a by-ref arg,
/// and the int return maps onto PSFS_*.
/// Mutable access to the stream's raw read state during a fill — the
/// resource RefCell is already mutably borrowed by the read fn, so
/// filters that capture or restore the stream position (consumed)
/// work through these fields directly. `fd` is the physical cursor
/// for fd-backed streams; `fraw`/`srbuf-style buf` cover the
/// in-memory stores. A None seek context means the caller is outside
/// a fill and `stream_seek_to`/`stream_pos_of` apply instead.
struct FillSeek<'a> {
    fd: Option<std::os::unix::io::RawFd>,
    pos: &'a mut u64,
    fraw: Option<&'a mut u64>,
    buf: &'a mut std::collections::VecDeque<u8>,
    eof: &'a mut bool,
}

/// A lightweight stand-in left in a stream's RefCell while the real
/// resource is detached for a filtered read: zend runs user filter
/// methods while the fill loop owns the stream's inner state, so
/// re-entered stream builtins must not observe a borrowed cell. Reads
/// on the busy stream short-circuit to false before they can see the
/// empty payload; meta/stat/blocking-style calls answer from the
/// preserved fields, and the real resource is swapped back afterwards.
fn stream_filter_standin(res: &PhpResource) -> Option<PhpResource> {
    let devnull = || std::fs::File::open("/dev/null").expect("open /dev/null");
    Some(match res {
        PhpResource::File {
            id,
            read,
            write,
            path,
            mode,
            ..
        } => PhpResource::File {
            id: *id,
            file: devnull(),
            read: *read,
            write: *write,
            pos: 0,
            eof: false,
            rbuf: Default::default(),
            rcap: 0,
            unlink_on_close: false,
            path: path.clone(),
            mode: mode.clone(),
        },
        PhpResource::Input { id, uri, mode, .. } => PhpResource::Input {
            id: *id,
            body: std::rc::Rc::new(Vec::new()),
            pos: 0,
            eof: false,
            pos_broken: false,
            uri: uri.clone(),
            mode: mode.clone(),
            spilled_fd: None,
            srbuf: Default::default(),
            rcap: 0,
            fraw: 0,
        },
        PhpResource::Mem {
            id,
            write,
            uri,
            mode,
            ..
        } => PhpResource::Mem {
            id: *id,
            buf: Vec::new(),
            pos: 0,
            eof: false,
            pos_broken: false,
            write: *write,
            append: false,
            uri: uri.clone(),
            mode: mode.clone(),
            temp_smax: None,
            spilled_fd: None,
            srbuf: Default::default(),
            rcap: 0,
            fraw: 0,
        },
        PhpResource::Pipe {
            id,
            write,
            socket,
            pty,
            nonblock,
            ..
        } => PhpResource::Pipe {
            id: *id,
            file: devnull(),
            write: *write,
            socket: *socket,
            pty: *pty,
            nonblock: *nonblock,
            pos: 0,
            eof: false,
            rbuf: Default::default(),
        },
        PhpResource::Stdio { id, which, .. } => PhpResource::Stdio {
            id: *id,
            which: *which,
            pos: 0,
        },
        _ => return None,
    })
}

fn user_filter_call(
    it: &mut Interp,
    obj: &Rc<RefCell<PhpObject>>,
    stream_id: u64,
    stream_val: &Value,
    input: Vec<u8>,
    closing: bool,
) -> Result<(i64, Vec<u8>), PhpError> {
    let in_id = it.next_res_id();
    let out_id = it.next_res_id();
    let mut in_brig = std::collections::VecDeque::new();
    // zend's closing/FEED_ME passes hand filter() an EMPTY input
    // brigade — no phantom zero-length bucket.
    if !input.is_empty() {
        in_brig.push_back(input);
    }
    it.stream_brigades.insert(in_id, in_brig);
    it.stream_brigades
        .insert(out_id, std::collections::VecDeque::new());
    // zend's userfilter.c hands ->stream the stream zval per call —
    // callers pass a NULL zval only for the destructor flush, when the
    // stream resource is already freed; unset again after the call.
    obj.borrow_mut()
        .props
        .insert("stream".into(), cell(stream_val.clone()));
    let prev_nofclose = it.filter_no_fclose;
    it.filter_no_fclose = Some(stream_id);
    // Reads re-entered on this stream fail while filter() runs (zend),
    // and the filter-context warn prefix survives builtins the
    // callback itself calls.
    let prev_ctx = it.filter_warn_ctx.clone();
    it.stream_filter_busy.insert(stream_id);
    let args = vec![
        cell(Value::Resource(Rc::new(RefCell::new(PhpResource::Other {
            id: in_id,
            kind: "userfilter.bucket brigade",
        })))),
        cell(Value::Resource(Rc::new(RefCell::new(PhpResource::Other {
            id: out_id,
            kind: "userfilter.bucket brigade",
        })))),
        // $consumed binds by-ref — the callee may bump it.
        cell(Value::Int(0)),
        cell(Value::Bool(closing)),
    ];
    let call = it.method_invoke(obj.clone(), "filter", CallArgs::positional(args));
    obj.borrow_mut()
        .props
        .insert("stream".into(), cell(Value::Null));
    it.filter_no_fclose = prev_nofclose;
    it.filter_warn_ctx = prev_ctx;
    it.stream_filter_busy.remove(&stream_id);
    let ret = match call {
        Ok(v) => v.to_int(),
        Err(e) => {
            // zend warns about buckets the failing filter left on the
            // input brigade before the exception propagates.
            let leftover = it.stream_brigades.remove(&in_id).unwrap_or_default();
            it.stream_brigades.remove(&out_id);
            if !leftover.is_empty() {
                let _ = it.warn_pub(&format!(
                    "{}(): Unprocessed filter buckets remaining on input brigade",
                    it.filter_warn_ctx
                ));
            }
            return Err(e);
        }
    };
    let leftover = it.stream_brigades.remove(&in_id).unwrap_or_default();
    if !leftover.is_empty() {
        it.warn_pub(&format!(
            "{}(): Unprocessed filter buckets remaining on input brigade",
            it.filter_warn_ctx
        ))?;
    }
    let out = it
        .stream_brigades
        .remove(&out_id)
        .unwrap_or_default()
        .into_iter()
        .flatten()
        .collect();
    Ok((ret, out))
}

/// One chain entry's transform. Returns (output, status) where
/// status is the PSFS code: 2=PASS_ON (output feeds the next entry),
/// 1=FEED_ME (filter wants more input; its output is discarded),
/// 0=ERR_FATAL (stream is borked; the fill fails).
#[allow(clippy::too_many_arguments)]
fn filter_apply_one(
    it: &mut Interp,
    sid: u64,
    idx: usize,
    stream_val: &Value,
    input: Vec<u8>,
    closing: bool,
    inc: bool,
    seek: &mut Option<FillSeek<'_>>,
) -> Result<(Vec<u8>, i64), PhpError> {
    let is_user = matches!(
        it.stream_filters.get(&sid).and_then(|v| v.get(idx)),
        Some(StreamFilter {
            state: FilterState::User(_),
            ..
        })
    );
    let is_codec = matches!(
        it.stream_filters.get(&sid).and_then(|v| v.get(idx)),
        Some(StreamFilter {
            state: FilterState::Codec(_),
            ..
        })
    );
    if is_codec {
        let key = it
            .stream_filters
            .get(&sid)
            .and_then(|v| v.get(idx))
            .map(|f| (f.fid, f.read))
            .unwrap_or((0, true));
        let Some(mut c) = it.codec_states.remove(&key) else {
            return Ok((input, 2));
        };
        let mode = if closing {
            CodecMode::Close
        } else if inc {
            CodecMode::Inc
        } else {
            CodecMode::Normal
        };
        let res = codec_run(&mut c, &input, mode);
        it.codec_states.insert(key, c);
        return match res {
            Ok(out) => Ok((out, 2)),
            Err(CodecErr::Zlib) => {
                it.notice_pub(&format!("{}(): zlib: data error", it.filter_warn_ctx))?;
                Ok((Vec::new(), 0))
            }
            Err(CodecErr::Bz) => {
                it.notice_pub(&format!(
                    "{}(): bzip2 decompression failed",
                    it.filter_warn_ctx
                ))?;
                Ok((Vec::new(), 0))
            }
        };
    }
    if is_user {
        let obj = match &it
            .stream_filters
            .get(&sid)
            .and_then(|v| v.get(idx))
            .unwrap()
            .state
        {
            FilterState::User(o) => o.clone(),
            _ => unreachable!(),
        };
        let (status, out) = user_filter_call(it, &obj, sid, stream_val, input, closing)?;
        return Ok((out, status));
    }
    let entry = &mut it.stream_filters.get_mut(&sid).unwrap()[idx];
    let mut buf = input;
    let mut status = 2i64;
    let mut deferred_warn: Option<String> = None;
    match &mut entry.state {
        FilterState::Plain => match entry.name.as_str() {
            "string.rot13" => {
                for b in buf.iter_mut() {
                    *b = match *b {
                        b'a'..=b'z' => b'a' + (*b - b'a' + 13) % 26,
                        b'A'..=b'Z' => b'A' + (*b - b'A' + 13) % 26,
                        _ => *b,
                    };
                }
            }
            "string.toupper" => {
                for b in buf.iter_mut() {
                    *b = b.to_ascii_uppercase();
                }
            }
            "string.tolower" => {
                for b in buf.iter_mut() {
                    *b = b.to_ascii_lowercase();
                }
            }
            _ => {}
        },
        FilterState::Codec(_) => unreachable!("codec handled above"),
        FilterState::Consumed { count, offset } => {
            if offset.is_none() {
                *offset = Some(
                    seek.as_ref()
                        .map(|s| *s.pos)
                        .unwrap_or_else(|| stream_pos_of(stream_val)),
                );
            }
            *count += buf.len() as u64;
            if closing {
                if let Some(off) = *offset {
                    let target = off + *count;
                    if let Some(s) = seek.as_mut() {
                        if let Some(fd) = s.fd {
                            unsafe {
                                libc::lseek(fd, target as libc::off_t, libc::SEEK_SET);
                            }
                        }
                        *s.pos = target;
                        if let Some(fr) = s.fraw.as_deref_mut() {
                            *fr = target;
                        }
                        s.buf.clear();
                        *s.eof = false;
                    } else {
                        stream_seek_to(stream_val, target);
                    }
                }
            }
        }
        FilterState::Dechunk(d) => php_dechunk(d, &mut buf),
        FilterState::Iconv {
            from,
            to,
            disp,
            pending,
            bom_done,
            translit,
        } => {
            // zend keeps a leftover input buffer across calls
            // (prevpent). On EILSEQ it holds the failed input only
            // when that buffer was already occupied — a fresh call's
            // bytes drop and the filter recovers, while a held
            // incomplete tail poisons the concat and every later
            // touch re-fails with the same warning.
            let had_pending = !pending.is_empty();
            pending.extend_from_slice(&buf);
            let (cps, used, mut invalid) = iconv_decode(&from.clone(), pending.as_slice());
            let mut encoded = None;
            if !invalid {
                encoded = iconv_encode(&to.clone(), &cps, !*bom_done, *translit);
                if encoded.is_none() {
                    // Unrepresentable output is EILSEQ too — zend
                    // warns and discards the whole brigade.
                    invalid = true;
                }
            }
            if !invalid {
                pending.drain(..used);
                buf = encoded.unwrap_or_default();
                if matches!(to.as_str(), "UTF16" | "UTF32") {
                    *bom_done = true;
                }
            }
            // A partial multibyte sequence still pending when the
            // chain closes is an invalid sequence too (zend EILSEQ).
            if !invalid && closing && !pending.is_empty() {
                invalid = true;
            }
            if invalid {
                if !had_pending {
                    pending.clear();
                }
                // zend's iconv filter warns + returns FEED_ME — the
                // brigade is discarded and the fill retries/ends.
                deferred_warn = Some(format!(
                    "{}(): iconv stream filter ({}): invalid multibyte sequence",
                    it.filter_warn_ctx, disp
                ));
                buf.clear();
                status = 1;
            }
        }
        FilterState::Base64 { decode, tail } => {
            buf = if *decode {
                b64_decode_filter(tail, &buf, closing)
            } else {
                b64_encode_filter(tail, &buf, closing)
            };
        }
        FilterState::Qp { encode, col, tail } => {
            buf = if *encode {
                qp_encode_filter(col, &buf)
            } else {
                qp_decode_filter(tail, &buf, closing)
            };
        }
        FilterState::User(_) => unreachable!(),
    }
    if let Some(w) = deferred_warn {
        it.warn_pub(&w)?;
    }
    Ok((buf, status))
}

/// The read/write chain for one direction — each applicable entry
/// transforms head→tail (zend's buckets_in→buckets_out walk). A
/// non-PASS_ON status breaks the walk and discards the output (zend:
/// brig_out is freed on FEED_ME/ERR_FATAL). Returns (data, status).
#[allow(clippy::too_many_arguments)]
fn run_filter_chain(
    it: &mut Interp,
    sid: u64,
    stream_val: &Value,
    read_dir: bool,
    raw: Vec<u8>,
    closing: bool,
    inc: bool,
    seek: &mut Option<FillSeek<'_>>,
) -> Result<(Vec<u8>, i64), PhpError> {
    let mut data = raw;
    // zend walks the LIVE chain (php_stream_filter_flush iterates
    // current->next): a user filter's filter() callback that attaches
    // another filter mid-pass has the appended entry drain the
    // remaining buckets, while a prepended one lands behind the
    // cursor and is never visited. Relocate our cursor by filter id
    // after each call so inserts/removes stay ordered like the
    // linked list — a removed entry continues at whatever shifted
    // into its slot.
    let mut i = 0usize;
    while let Some((fid, applies)) = it
        .stream_filters
        .get(&sid)
        .and_then(|v| v.get(i))
        .map(|f| (f.fid, if read_dir { f.read } else { f.write }))
    {
        if !applies {
            i += 1;
            continue;
        }
        let (out, status) = filter_apply_one(it, sid, i, stream_val, data, closing, inc, seek)?;
        if status != 2 {
            return Ok((Vec::new(), status));
        }
        data = out;
        i = it
            .stream_filters
            .get(&sid)
            .and_then(|v| {
                v.iter()
                    .position(|f| f.fid == fid && (if read_dir { f.read } else { f.write }))
            })
            .map(|p| p + 1)
            .unwrap_or(i);
    }
    Ok((data, 2))
}

/// zend's _php_stream_do_seek runs the write-filter chain once with
/// FLUSH_INC before the seek op — pending compressed data must land
/// in the backing store for the new position to make sense.
fn stream_seek_flush(it: &mut Interp, r: &Rc<RefCell<PhpResource>>) -> Result<(), PhpError> {
    let rid = r.borrow().id();
    if !stream_write_filtered(it, rid) {
        return Ok(());
    }
    let sv = Value::Resource(r.clone());
    let (out, _) = run_filter_chain(it, rid, &sv, false, Vec::new(), false, true, &mut None)?;
    if !out.is_empty() {
        let _ = write_resource_raw(it, r, &out)?;
    }
    Ok(())
}

fn stream_read_filtered(it: &Interp, sid: u64) -> bool {
    it.stream_filters
        .get(&sid)
        .is_some_and(|v| v.iter().any(|f| f.read))
}

fn stream_write_filtered(it: &Interp, sid: u64) -> bool {
    it.stream_filters
        .get(&sid)
        .is_some_and(|v| v.iter().any(|f| f.write))
}

/// The stream's current logical position — consumed filter's
/// offset capture.
fn stream_pos_of(v: &Value) -> u64 {
    let Value::Resource(r) = v else { return 0 };
    let rb = r.borrow();
    match &*rb {
        PhpResource::File { pos, .. }
        | PhpResource::Mem { pos, .. }
        | PhpResource::Input { pos, .. }
        | PhpResource::Pipe { pos, .. } => *pos,
        _ => 0,
    }
}

/// The consumed filter's closing seek — restore the store cursor to
/// offset+consumed.
fn stream_seek_to(v: &Value, target: u64) {
    let Value::Resource(r) = v else { return };
    let mut rb = r.borrow_mut();
    match &mut *rb {
        PhpResource::File {
            file,
            pos,
            eof,
            rbuf,
            ..
        } => {
            use std::io::{Seek, SeekFrom};
            if let Ok(newp) = file.seek(SeekFrom::Start(target)) {
                *pos = newp;
                *eof = false;
                rbuf.clear();
            }
        }
        PhpResource::Mem {
            pos,
            eof,
            fraw,
            srbuf,
            ..
        } => {
            *pos = target;
            *fraw = target;
            *eof = false;
            srbuf.clear();
        }
        PhpResource::Input {
            pos,
            eof,
            fraw,
            srbuf,
            ..
        } => {
            *pos = target;
            *fraw = target;
            *eof = false;
            srbuf.clear();
        }
        PhpResource::Pipe { pos, eof, rbuf, .. } => {
            *pos = target;
            *eof = false;
            rbuf.clear();
        }
        _ => {}
    }
}

/// The resource's open-mode string (zend stream->mode) — the
/// mode==0 attach derivation reads it.
fn stream_open_mode(res: &PhpResource) -> String {
    match res {
        PhpResource::File { mode, .. } => mode.clone(),
        PhpResource::Mem { mode, .. } => mode.clone(),
        PhpResource::Input { mode, .. } => mode.clone(),
        PhpResource::Stdio { which, .. } => {
            if *which == 0 {
                "rb".into()
            } else {
                "wb".into()
            }
        }
        PhpResource::Pipe {
            write, socket, pty, ..
        } => {
            if *socket || *pty {
                "r+".into()
            } else if *write {
                "w".into()
            } else {
                "r".into()
            }
        }
        _ => String::new(),
    }
}

/// stream_filter_append/prepend — zend's php_stream_filter_attach.
fn stream_filter_attach(
    it: &mut Interp,
    args: &[Cell],
    fname: &str,
    prepend: bool,
) -> Result<Value, PhpError> {
    if args.len() < 2 {
        return err(
            "ArgumentCountError",
            format!(
                "{fname}() expects at least 2 arguments, {} given",
                args.len()
            ),
        );
    }
    if args.len() > 4 {
        return err(
            "ArgumentCountError",
            format!(
                "{fname}() expects at most 4 arguments, {} given",
                args.len()
            ),
        );
    }
    stream_open_check(args, 0, fname, 1, "stream")?;
    let Value::Resource(r) = &*args[0].borrow() else {
        unreachable!()
    };
    let sid = r.borrow().id();
    let filter = zpp_strict_string(it, args, 1, fname, "$filter_name")?;
    let mode = zpp_long_arg(it, args, 2, fname, 3, "$mode")?;
    // mode==0 derives the chains from the stream's open mode
    // (filter.c: 'r' or '+' → READ; 'w', 'a' or '+' → WRITE; other
    // open modes like 'x'/'c' leave mask 0 → silent failure).
    let mut mask = mode & 3;
    if mask == 0 {
        let m = stream_open_mode(&r.borrow());
        if m.contains('r') || m.contains('+') {
            mask |= 1;
        }
        if m.contains('w') || m.contains('+') || m.contains('a') {
            mask |= 2;
        }
    }
    if mask == 0 {
        return Ok(Value::Bool(false));
    }
    let params = args.get(3);
    let stream_val = Value::Resource(r.clone());
    let mut bound_fid = 0u64;
    // STREAM_FILTER_ALL creates one instance PER chain (zend binds
    // the resource to the last instantiated — the write one).
    for dir in [FILTER_READ, false] {
        let on = if dir { mask & 1 != 0 } else { mask & 2 != 0 };
        if !on {
            continue;
        }
        let entry = match filter_instantiate(it, &stream_val, &filter, params) {
            Ok(Some(e)) => e,
            Ok(None) => return Ok(Value::Bool(false)),
            Err(e) => return Err(e),
        };
        let mut entry = entry;
        // zend binds ONE filter resource per attach call — an ALL-mode
        // attach still burns a single res id (the write instance's).
        if bound_fid == 0 {
            bound_fid = it.next_res_id();
        }
        entry.fid = bound_fid;
        let fid = bound_fid;
        entry.read = dir;
        entry.write = !dir;
        if let FilterState::Codec(kind) = entry.state {
            it.codec_states.insert((fid, dir), CodecState::new(kind));
        }
        {
            let chain = it.stream_filters.entry(sid).or_default();
            if prepend {
                chain.insert(0, entry);
            } else {
                chain.push(entry);
            }
        }
        bound_fid = fid;
        // appending to the READ chain re-filters already-buffered
        // bytes through only the new filter (filter.c:303-387);
        // prepend does not.
        if dir && !prepend {
            refilter_pending(it, sid, stream_val.clone(), fid)?;
        }
    }
    let fres = Rc::new(RefCell::new(PhpResource::Other {
        id: bound_fid,
        kind: "stream filter",
    }));
    it.stream_filter_bindings
        .insert(bound_fid, (sid, filter, fres.clone(), r.clone()));
    Ok(Value::Resource(fres))
}

/// Factory-resolve + instantiate one chain entry; None → create
/// failed (warn already emitted).
fn filter_instantiate(
    it: &mut Interp,
    stream_val: &Value,
    name: &str,
    params: Option<&Cell>,
) -> Result<Option<StreamFilter>, PhpError> {
    match filter_resolve(it, name) {
        None => {
            it.warn_pub(&format!(
                "{}(): Unable to locate filter \"{name}\"",
                it.filter_warn_ctx
            ))?;
            Ok(None)
        }
        Some(FactoryHit::State(Ok(state))) => {
            Ok(Some(StreamFilter {
                name: name.to_string(),
                read: false,
                write: false,
                // zend assigns the resource id only on success —
                // the caller burns it once the entry exists.
                fid: 0,
                state,
            }))
        }
        Some(FactoryHit::State(Err(_))) => {
            it.warn_pub(&format!(
                "{}(): Unable to create or locate filter \"{name}\"",
                it.filter_warn_ctx
            ))?;
            Ok(None)
        }
        Some(FactoryHit::Class(cls)) => {
            if it.lookup_class(&cls).is_none() {
                it.run_autoload(&cls)?;
            }
            if it.lookup_class(&cls).is_none() {
                it.warn_pub(&format!(
                    "{}(): User-filter \"{name}\" requires class \"{cls}\", but that class is not defined",
                    it.filter_warn_ctx
                ))?;
                it.warn_pub(&format!(
                    "{}(): Unable to create or locate filter \"{name}\"",
                    it.filter_warn_ctx
                ))?;
                return Ok(None);
            }
            let Value::Object(obj) = it.instantiate(&cls.to_lowercase(), &[])? else {
                unreachable!()
            };
            // $filtername/$params get the attach args; ->stream stays
            // null outside filter() calls (zend).
            obj.borrow_mut()
                .props
                .insert("filtername".into(), cell(Value::str(name)));
            let pv = params.map(|c| c.borrow().clone()).unwrap_or(Value::Null);
            obj.borrow_mut().props.insert("params".into(), cell(pv));
            let _ = stream_val;
            let ok = it
                .method_invoke(obj.clone(), "onCreate", CallArgs::empty())
                .map(|v| v.is_truthy())
                .unwrap_or(false);
            if !ok {
                it.warn_pub(&format!(
                    "{}(): Unable to create or locate filter \"{name}\"",
                    it.filter_warn_ctx
                ))?;
                return Ok(None);
            }
            Ok(Some(StreamFilter {
                name: name.to_string(),
                read: false,
                write: false,
                fid: 0,
                state: FilterState::User(obj),
            }))
        }
    }
}

/// After an append to the read chain, buffered bytes re-run through
/// only the new entry (zend wraps readpos..writepos in one bucket).
fn refilter_pending(
    it: &mut Interp,
    sid: u64,
    stream_val: Value,
    new_fid: u64,
) -> Result<(), PhpError> {
    let idx = it
        .stream_filters
        .get(&sid)
        .and_then(|v| v.iter().position(|f| f.fid == new_fid));
    let Some(idx) = idx else { return Ok(()) };
    let Value::Resource(r) = &stream_val else {
        return Ok(());
    };
    // drain the pending filtered buffer for this stream
    let pending: Vec<u8> = {
        let mut rb = r.borrow_mut();
        match &mut *rb {
            PhpResource::File { rbuf, .. } | PhpResource::Pipe { rbuf, .. } => {
                rbuf.drain(..).collect()
            }
            PhpResource::Mem { srbuf, .. } | PhpResource::Input { srbuf, .. } => {
                srbuf.drain(..).collect()
            }
            _ => Vec::new(),
        }
    };
    if pending.is_empty() {
        return Ok(());
    }
    let (out, _) = filter_apply_one(it, sid, idx, &stream_val, pending, false, false, &mut None)?;
    let mut rb = r.borrow_mut();
    match &mut *rb {
        PhpResource::File { rbuf, .. } | PhpResource::Pipe { rbuf, .. } => rbuf.extend(out),
        PhpResource::Mem { srbuf, .. } | PhpResource::Input { srbuf, .. } => srbuf.extend(out),
        _ => {}
    }
    Ok(())
}

/// zend fill_read_buffer's filtered path: raw chunk reads keep
/// landing until the buffered count reaches to_read_now or the store
/// dries up; every chunk passes through the read chain BEFORE the
/// buffer (zend buffers FILTERED bytes). got==0 latches stream->eof
/// and runs the chain once with the closing flag.
#[allow(clippy::too_many_arguments)]
fn fd_filtered_fill(
    it: &mut Interp,
    sid: u64,
    stream_val: &Value,
    fd: std::os::unix::io::RawFd,
    pos: &mut u64,
    fraw: Option<&mut u64>,
    eof: &mut bool,
    rbuf: &mut std::collections::VecDeque<u8>,
    chunk: usize,
    to_read_now: usize,
) -> Result<bool, PhpError> {
    let mut seek = Some(FillSeek {
        fd: Some(fd),
        pos,
        fraw,
        buf: rbuf,
        eof,
    });
    while !seek.as_ref().map(|s| *s.eof).unwrap_or(true)
        && seek.as_ref().map(|s| s.buf.len()).unwrap_or(0) < to_read_now
    {
        let mut buf = vec![0u8; chunk.max(1)];
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        let (raw, last) = if n < 0 {
            // zend treats read errors (incl. EAGAIN on nonblocking)
            // like a dry read: chain runs once non-closing, fill ends.
            (Vec::new(), true)
        } else if n == 0 {
            *seek.as_mut().unwrap().eof = true;
            (Vec::new(), true)
        } else {
            buf.truncate(n as usize);
            (buf, false)
        };
        let closing = seek.as_ref().map(|s| *s.eof).unwrap_or(true);
        let (out, status) =
            run_filter_chain(it, sid, stream_val, true, raw, closing, false, &mut seek)?;
        if status == 0 {
            // PSFS_ERR_FATAL — stream->eof + fatal_error, fill FAILURE.
            *seek.as_mut().unwrap().eof = true;
            return Ok(true);
        }
        if status == 2 {
            let sk = seek.as_mut().unwrap();
            sk.buf.extend(out);
        }
        // PSFS_FEED_ME: discard the output and keep looping.
        if last {
            break;
        }
    }
    Ok(false)
}

/// fd_stream_read's filtered variant — the drain→fill loop zend's
/// _php_stream_read runs for the FILE-family streams (plain/STDIO/
/// spilled temp): it drains then refills until the request is met,
/// the store dies, or a filter reports fatal.
#[allow(clippy::too_many_arguments)]
fn fd_stream_read_filtered(
    it: &mut Interp,
    sid: u64,
    stream_val: &Value,
    fd: std::os::unix::io::RawFd,
    pos: &mut u64,
    eof: &mut bool,
    rbuf: &mut std::collections::VecDeque<u8>,
    chunk: usize,
    n: usize,
) -> Result<StreamRead, PhpError> {
    let mut out: Vec<u8> = Vec::new();
    let mut fatal = false;
    // zend _php_stream_read: drain the FILTERED buffer first (even
    // at eof — pending closing-flush bytes still deliver), then fill.
    while out.len() < n {
        out.extend(rbuf.drain(..(n - out.len()).min(rbuf.len())));
        if out.len() >= n || *eof || fatal {
            break;
        }
        let to_read_now = (n - out.len()).min(chunk);
        fatal = fd_filtered_fill(
            it,
            sid,
            stream_val,
            fd,
            pos,
            None,
            eof,
            rbuf,
            chunk,
            to_read_now,
        )?;
    }
    *pos += out.len() as u64;
    if fatal && out.is_empty() {
        Ok(StreamRead::FailSilent)
    } else {
        Ok(StreamRead::Data(out))
    }
}

/// The same fill for php://memory / php://temp* / data: /
/// php://input bodies — raw bytes slice from `fraw`, delivered bytes
/// buffer in `srbuf` while `pos` stays the delivered count.
#[allow(clippy::too_many_arguments)]
fn mem_filtered_read(
    it: &mut Interp,
    sid: u64,
    stream_val: &Value,
    store: &[u8],
    fraw: &mut u64,
    pos: &mut u64,
    eof: &mut bool,
    srbuf: &mut std::collections::VecDeque<u8>,
    chunk: usize,
    n: usize,
) -> Result<StreamRead, PhpError> {
    let mut out: Vec<u8> = Vec::new();
    let mut fatal = false;
    while out.len() < n {
        out.extend(srbuf.drain(..(n - out.len()).min(srbuf.len())));
        if out.len() >= n || *eof || fatal {
            break;
        }
        let to_read_now = (n - out.len()).min(chunk);
        while !*eof && srbuf.len() < to_read_now {
            let start = (*fraw as usize).min(store.len());
            // zend's mem-stream read latches eof as soon as the store
            // is fully consumed — the last DATA pass already carries
            // the closing flag.
            let (raw, last) = if start >= store.len() {
                *eof = true;
                (Vec::new(), true)
            } else {
                let got = (store.len() - start).min(chunk);
                let b = store[start..start + got].to_vec();
                *fraw += got as u64;
                if start + got >= store.len() {
                    *eof = true;
                    (b, true)
                } else {
                    (b, false)
                }
            };
            let closing = *eof;
            let mut seek = Some(FillSeek {
                fd: None,
                pos: &mut *pos,
                fraw: Some(&mut *fraw),
                buf: &mut *srbuf,
                eof: &mut *eof,
            });
            let (filtered, status) =
                run_filter_chain(it, sid, stream_val, true, raw, closing, false, &mut seek)?;
            if status == 0 {
                *eof = true;
                fatal = true;
                break;
            }
            if status == 2 {
                srbuf.extend(filtered);
            }
            if last {
                break;
            }
        }
        if fatal {
            break;
        }
    }
    *pos += out.len() as u64;
    if fatal && out.is_empty() {
        Ok(StreamRead::FailSilent)
    } else {
        Ok(StreamRead::Data(out))
    }
}

/// Filtered variant of fd_line_read — buffered+filtered fills.
#[allow(clippy::too_many_arguments)]
fn fd_line_read_filtered(
    it: &mut Interp,
    sid: u64,
    stream_val: &Value,
    fd: std::os::unix::io::RawFd,
    pos: &mut u64,
    eof: &mut bool,
    rbuf: &mut std::collections::VecDeque<u8>,
    chunk: usize,
    limit: usize,
) -> Result<StreamRead, PhpError> {
    let mut out: Vec<u8> = Vec::new();
    let mut fatal = false;
    while out.len() < limit {
        let take = (limit - out.len()).min(rbuf.len());
        let mut i = 0;
        let mut hit = false;
        while i < take {
            let b = rbuf.pop_front().unwrap();
            i += 1;
            out.push(b);
            if b == b'\n' {
                hit = true;
                break;
            }
        }
        if hit || *eof || fatal {
            break;
        }
        let to_read_now = (limit - out.len()).min(chunk);
        fatal = fd_filtered_fill(
            it,
            sid,
            stream_val,
            fd,
            pos,
            None,
            eof,
            rbuf,
            chunk,
            to_read_now,
        )?;
    }
    *pos += out.len() as u64;
    if fatal && out.is_empty() {
        Ok(StreamRead::FailSilent)
    } else {
        Ok(StreamRead::Data(out))
    }
}

/// Filtered variant of read_line_pipe / the Pipe one-shot read:
/// drain once, a single fill, drain again — zend doesn't re-fill
/// pipes inside one user call. `nl` selects fgets (stop at '\n')
/// vs fread (byte count only).
#[allow(clippy::too_many_arguments)]
fn pipe_line_read_filtered(
    it: &mut Interp,
    sid: u64,
    stream_val: &Value,
    fd: std::os::unix::io::RawFd,
    pos: &mut u64,
    eof: &mut bool,
    rbuf: &mut std::collections::VecDeque<u8>,
    chunk: usize,
    limit: usize,
    nl: bool,
) -> Result<StreamRead, PhpError> {
    let mut out: Vec<u8> = Vec::new();
    let take = (limit - out.len()).min(rbuf.len());
    for _ in 0..take {
        let b = rbuf.pop_front().unwrap();
        out.push(b);
        if nl && b == b'\n' {
            *pos += out.len() as u64;
            return Ok(StreamRead::Data(out));
        }
    }
    if out.len() < limit && !*eof {
        let to_read_now = (limit - out.len()).min(chunk);
        let fatal = fd_filtered_fill(
            it,
            sid,
            stream_val,
            fd,
            pos,
            None,
            eof,
            rbuf,
            chunk,
            to_read_now,
        )?;
        let take = (limit - out.len()).min(rbuf.len());
        for _ in 0..take {
            let b = rbuf.pop_front().unwrap();
            out.push(b);
            if nl && b == b'\n' {
                break;
            }
        }
        *pos += out.len() as u64;
        if fatal && out.is_empty() {
            return Ok(StreamRead::FailSilent);
        }
        return Ok(StreamRead::Data(out));
    }
    *pos += out.len() as u64;
    Ok(StreamRead::Data(out))
}

/// fread on a filtered pipe — pipe_line_read_filtered with nl=false.
#[allow(clippy::too_many_arguments)]
fn pipe_read_filtered(
    it: &mut Interp,
    sid: u64,
    stream_val: &Value,
    fd: std::os::unix::io::RawFd,
    pos: &mut u64,
    eof: &mut bool,
    rbuf: &mut std::collections::VecDeque<u8>,
    chunk: usize,
    n: usize,
) -> Result<StreamRead, PhpError> {
    pipe_line_read_filtered(it, sid, stream_val, fd, pos, eof, rbuf, chunk, n, false)
}

/// zend zpp 's' for params that must be a real string: scalars
/// coerce, everything else TypeErrors.
fn zpp_strict_string(
    it: &mut Interp,
    args: &[Cell],
    i: usize,
    fname: &str,
    pname: &str,
) -> Result<String, PhpError> {
    match arg(args, i) {
        Value::Str(s) => Ok(String::from_utf8_lossy(&s).into_owned()),
        Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Null => Ok(arg_str(it, args, i)),
        v => err(
            "TypeError",
            format!(
                "{fname}(): Argument #{p} (${pname}) must be of type string, {} given",
                zval_word(&v),
                p = i + 1,
            ),
        ),
    }
}

/// Build a StreamBucket object wrapping `bytes` — stream_bucket_new()
/// and stream_bucket_make_writeable() share it.
fn new_stream_bucket(it: &mut Interp, bytes: Vec<u8>) -> Result<Value, PhpError> {
    let Value::Object(obj) = it.instantiate("streambucket", &[])? else {
        unreachable!()
    };
    let bid = it.next_res_id();
    it.stream_buckets.insert(bid, bytes.clone());
    {
        let mut o = obj.borrow_mut();
        o.props.insert(
            "bucket".into(),
            cell(Value::Resource(Rc::new(RefCell::new(PhpResource::Other {
                id: bid,
                kind: "userfilter.bucket",
            })))),
        );
        o.props
            .insert("data".into(), cell(Value::bytes(bytes.clone())));
        let n = Value::Int(bytes.len() as i64);
        o.props.insert("datalen".into(), cell(n.clone()));
        o.props.insert("dataLength".into(), cell(n));
    }
    Ok(Value::Object(obj))
}

/// stream_filter_remove — zend's php_stream_filter_remove: flush the
/// entry once with the closing flag, then unlink it and run a user
/// filter's onClose. A non-PASS_ON flush keeps the entry attached
/// with 'Unable to flush filter, not removing'.
fn stream_filter_remove(it: &mut Interp, args: &[Cell]) -> Result<Value, PhpError> {
    if args.len() != 1 {
        return err(
            "ArgumentCountError",
            format!(
                "stream_filter_remove() expects exactly 1 argument, {} given",
                args.len()
            ),
        );
    }
    let Value::Resource(r) = arg(args, 0) else {
        return err(
            "TypeError",
            format!(
                "stream_filter_remove(): Argument #1 ($stream_filter) must be of type resource, {} given",
                zval_word(&arg(args, 0))
            ),
        );
    };
    let fid = r.borrow().id();
    let Some((sid, _fname, _fres, sres)) = it.stream_filter_bindings.get(&fid).cloned() else {
        return err(
            "TypeError",
            "stream_filter_remove(): supplied resource is not a valid stream filter resource",
        );
    };
    let idx = it.stream_filters.get(&sid).and_then(|v| {
        // an ALL-mode attach shares one bound res id across its
        // read+write entries — the resource binds the write one.
        v.iter()
            .position(|f| f.fid == fid && f.write)
            .or_else(|| v.iter().position(|f| f.fid == fid))
    });
    let Some(idx) = idx else {
        return err(
            "TypeError",
            "stream_filter_remove(): supplied resource is not a valid stream filter resource",
        );
    };
    // flush: one call with an empty brigade + PSFS_FLAG_FLUSH_CLOSE —
    // consumed seeks back, user filters see $closing=true.
    let sv = Value::Resource(sres);
    let (_out, status) = filter_apply_one(it, sid, idx, &sv, Vec::new(), true, false, &mut None)?;
    if status != 2 {
        it.warn_pub(&format!(
            "{}(): Unable to flush filter, not removing",
            it.filter_warn_ctx
        ))?;
        return Ok(Value::Bool(false));
    }
    if let Some(v) = it.stream_filters.get_mut(&sid) {
        // unlink first; a php_user_filter dtor calls onClose after.
        let entry = v.remove(idx);
        it.codec_states.remove(&(entry.fid, entry.read));
        if let FilterState::User(obj) = entry.state {
            let _ = it.method_invoke(obj, "onClose", CallArgs::empty());
        }
    }
    it.stream_filter_bindings.remove(&fid);
    let mut rb = r.borrow_mut();
    *rb = PhpResource::Closed { id: fid };
    Ok(Value::Bool(true))
}

/// zend's resource-list teardown for one filtered stream, run at
/// request shutdown: the write chain flushes with ->stream NULL (the
/// stream zval is already freed), each php_user_filter gets onClose,
/// held filter resources die, and the stream goes 'resource (closed)'.
pub(crate) fn stream_dtor_flush(
    it: &mut Interp,
    r: &Rc<RefCell<PhpResource>>,
) -> Result<(), PhpError> {
    let sid = r.borrow().id();
    let Some(filters) = it.stream_filters.get(&sid).cloned() else {
        return Ok(());
    };
    // The closing flush's error must not bail before teardown —
    // php_stream_free completes under a pending exception, and a
    // half-torn chain re-runs the close at the next teardown pass.
    let mut close_err: Option<PhpError> = None;
    if filters.iter().any(|f| f.write) {
        match run_filter_chain(
            it,
            sid,
            &Value::Null,
            false,
            Vec::new(),
            true,
            false,
            &mut None,
        ) {
            Ok((out, _)) => {
                if !out.is_empty() {
                    let _ = write_resource_raw(it, r, &out);
                }
            }
            Err(e) => close_err = Some(e),
        }
    }
    for f in &filters {
        if let FilterState::User(obj) = &f.state {
            let _ = it.method_invoke(obj.clone(), "onClose", CallArgs::empty());
        }
    }
    *r.borrow_mut() = PhpResource::Closed { id: sid };
    if let Some(dead_filters) = it.stream_filters.remove(&sid) {
        for f in dead_filters {
            it.codec_states.remove(&(f.fid, f.read));
        }
    }
    let dead: Vec<u64> = it
        .stream_filter_bindings
        .iter()
        .filter(|(_, (s, _, _, _))| *s == sid)
        .map(|(fid, _)| *fid)
        .collect();
    for fid in dead {
        if let Some((_, _, fres, _)) = it.stream_filter_bindings.remove(&fid) {
            let mut fb = fres.borrow_mut();
            *fb = PhpResource::Closed { id: fid };
        }
    }
    match close_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Filtered fgets over an in-memory store (Mem/Input unspilled).
#[allow(clippy::too_many_arguments)]
fn mem_line_read_filtered(
    it: &mut Interp,
    sid: u64,
    stream_val: &Value,
    store: &[u8],
    fraw: &mut u64,
    pos: &mut u64,
    eof: &mut bool,
    srbuf: &mut std::collections::VecDeque<u8>,
    chunk: usize,
    limit: usize,
) -> Result<StreamRead, PhpError> {
    let mut out: Vec<u8> = Vec::new();
    let mut fatal = false;
    while out.len() < limit {
        let take = (limit - out.len()).min(srbuf.len());
        let mut hit = false;
        for _ in 0..take {
            let b = srbuf.pop_front().unwrap();
            out.push(b);
            if b == b'\n' {
                hit = true;
                break;
            }
        }
        if hit || *eof || fatal {
            break;
        }
        let to_read_now = (limit - out.len()).min(chunk);
        while !*eof && srbuf.len() < to_read_now {
            let start = (*fraw as usize).min(store.len());
            // Last-data pass latches eof like zend's mem read.
            let (raw, last) = if start >= store.len() {
                *eof = true;
                (Vec::new(), true)
            } else {
                let got = (store.len() - start).min(chunk);
                let b = store[start..start + got].to_vec();
                *fraw += got as u64;
                if start + got >= store.len() {
                    *eof = true;
                    (b, true)
                } else {
                    (b, false)
                }
            };
            let closing = *eof;
            let mut seek = Some(FillSeek {
                fd: None,
                pos: &mut *pos,
                fraw: Some(&mut *fraw),
                buf: &mut *srbuf,
                eof: &mut *eof,
            });
            let (filtered, status) =
                run_filter_chain(it, sid, stream_val, true, raw, closing, false, &mut seek)?;
            if status == 0 {
                *eof = true;
                fatal = true;
                break;
            }
            if status == 2 {
                srbuf.extend(filtered);
            }
            if last {
                break;
            }
        }
        if fatal {
            break;
        }
    }
    *pos += out.len() as u64;
    if fatal && out.is_empty() {
        Ok(StreamRead::FailSilent)
    } else {
        Ok(StreamRead::Data(out))
    }
}

/// zend's non-FOR_SELECT buffer sync (cast.c: php_stream_flush +
/// ops->seek(position) + readpos=writepos=0, and the same
/// discard+reseek a buffered write does first, streams.c:1194):
/// the fd's kernel offset is lseek(2)'d back to the stream's logical
/// position — unconditionally, since a child holding a dup may have
/// moved it while our read buffer was empty — and pending read-buffer
/// bytes are dropped. FOR_SELECT casts must NOT do this — they keep
/// the read buffer.
pub(in crate::builtins) fn fd_resync(
    fd: std::os::unix::io::RawFd,
    pos: u64,
    srbuf: &mut std::collections::VecDeque<u8>,
) {
    unsafe {
        libc::lseek(fd, pos as libc::off_t, libc::SEEK_SET);
    }
    srbuf.clear();
}

/// The resync above, resolved against a spilled Mem/Input stream.
pub(in crate::builtins) fn resync_spilled_fd(res: &mut PhpResource, fd: std::os::unix::io::RawFd) {
    match res {
        PhpResource::Mem { pos, srbuf, .. } | PhpResource::Input { pos, srbuf, .. } => {
            fd_resync(fd, *pos, srbuf)
        }
        _ => {}
    }
}

/// Bytes pending in a spilled stream's read buffer — what zend's
/// "N bytes of buffered data lost during stream conversion!" counts.
pub(in crate::builtins) fn spilled_buffered(res: &PhpResource) -> usize {
    match res {
        PhpResource::Mem {
            spilled_fd: Some(_),
            srbuf,
            ..
        }
        | PhpResource::Input {
            spilled_fd: Some(_),
            srbuf,
            ..
        } => srbuf.len(),
        _ => 0,
    }
}

/// zend's PHP_STREAM_AS_FD_FOR_SELECT cast on a buffer-backed stream:
/// the buffer is spilled into a tmpfile() positioned at the stream's
/// offset and the stream KEEPS the fd — later casts reuse it and
/// flock(2)/fstat(2) see it. php://temp* and data: are castable;
/// php://memory and php://input are not.
pub(in crate::builtins) fn spill_fd_for_stream(
    res: &mut PhpResource,
) -> Option<std::os::unix::io::RawFd> {
    match res {
        PhpResource::Mem {
            buf,
            pos,
            spilled_fd,
            temp_smax,
            ..
        } if temp_smax.is_some() => {
            if spilled_fd.is_none() {
                *spilled_fd = temp_spill_fd(buf, *pos);
            }
            *spilled_fd
        }
        PhpResource::Input {
            uri,
            body,
            pos,
            spilled_fd,
            ..
        } if is_data_uri(uri) => {
            if spilled_fd.is_none() {
                *spilled_fd = temp_spill_fd(body, *pos);
            }
            *spilled_fd
        }
        _ => None,
    }
}

/// The stream's descriptor when it already has one — no spilling.
/// Shared by stream_isatty/posix_isatty/flock.
fn existing_fd(res: &PhpResource) -> Option<std::os::unix::io::RawFd> {
    use std::os::fd::AsRawFd;
    match res {
        PhpResource::File { file, .. } | PhpResource::Pipe { file, .. } => Some(file.as_raw_fd()),
        PhpResource::Stdio { which, .. } if *which <= 2 => Some(*which as i32),
        PhpResource::Mem { spilled_fd, .. } | PhpResource::Input { spilled_fd, .. } => *spilled_fd,
        _ => None,
    }
}

/// zend_zval_type_name for the `?array` TypeError: objects render as
/// their class name, bool as `bool` (zend reports IS_TRUE/IS_FALSE
/// uniformly).
fn select_arg_word(v: &Value) -> String {
    match v {
        Value::Bool(_) => "bool".into(),
        _ => zval_word(v),
    }
}
