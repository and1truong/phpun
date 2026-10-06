//! Process-exec builtins: proc_open() over real child processes
//! (std::process + raw fd plumbing matching zend's proc_open.c), the
//! `/bin/sh -c` exec family, and shell-quoting helpers. Unix only.

use super::string::zpp_long_arg;
use super::*;

use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
    Ok(Some(match name {
        "proc_open" => proc_open(it, args)?,
        "proc_close" => proc_close(it, args)?,
        "proc_get_status" => proc_get_status(it, args)?,
        "proc_terminate" => proc_terminate(it, args)?,
        "proc_nice" => {
            if args.len() != 1 {
                return err(
                    "ArgumentCountError",
                    format!(
                        "proc_nice() expects exactly 1 argument, {} given",
                        args.len()
                    ),
                );
            }
            let n = zpp_long_arg(it, args, 0, "proc_nice", 1, "$priority")? as i32;
            unsafe {
                *libc::__errno_location() = 0;
                let ret = libc::nice(n);
                if ret == -1 && *libc::__errno_location() != 0 {
                    // EPERM — nice() also fails this way for an
                    // unprivileged priority increase.
                    it.warn_pub(
                        "proc_nice(): Only a super user may attempt to increase the priority of a process",
                    )?;
                    Value::Bool(false)
                } else {
                    Value::Bool(true)
                }
            }
        }
        "shell_exec" => shell_exec(it, args)?,
        "exec" => php_exec(it, name, args, if args.len() >= 2 { 2 } else { 0 })?,
        "system" => php_exec(it, name, args, 1)?,
        "passthru" => php_exec(it, name, args, 3)?,
        "escapeshellarg" => escape_shell_arg(it, args)?,
        "escapeshellcmd" => escape_shell_cmd(it, args)?,
        _ => return Ok(None),
    }))
}

fn to_os(b: &[u8]) -> &OsStr {
    OsStr::from_bytes(b)
}

/// exec arguments are NUL-terminated C strings — zend silently
/// truncates at the first NUL byte there, so do the same.
fn trunc_nul(b: &[u8]) -> &[u8] {
    match b.iter().position(|&c| c == 0) {
        Some(i) => &b[..i],
        None => b,
    }
}

/// execvp-style argv[0] resolution in the CALLER's PATH — zend
/// resolves the program in the parent before the child's $env is
/// installed, so an $env without PATH (or empty) can't break the
/// lookup. Falls back to the literal name when nothing resolves.
fn resolve_argv0(prog: &[u8]) -> Vec<u8> {
    if prog.contains(&b'/') {
        return prog.to_vec();
    }
    let Some(path) = std::env::var_os("PATH") else {
        return prog.to_vec();
    };
    use std::os::unix::fs::PermissionsExt;
    for dir in std::env::split_paths(&path) {
        // Empty PATH entries mean the current directory (execvp).
        let cand = if dir.as_os_str().is_empty() {
            std::path::Path::new(".").join(to_os(prog))
        } else {
            dir.join(to_os(prog))
        };
        let ok = cand
            .metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
        if ok {
            return cand.into_os_string().as_bytes().to_vec();
        }
    }
    prog.to_vec()
}

fn io_err_str(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(n) => unsafe {
            std::ffi::CStr::from_ptr(libc::strerror(n))
                .to_string_lossy()
                .into_owned()
        },
        None => e.to_string(),
    }
}

/// Children inherit the putenv() environment: zend's putenv mutates
/// the real environ, which every spawned child picks up. Our
/// overrides stand in for that — sets become env(), unsets become
/// env_remove() on top of the inherited environ.
fn apply_env_overrides(it: &Interp, cmd: &mut Command) {
    for (k, v) in it.env_overrides_pub() {
        match v {
            Some(v) => {
                cmd.env(k, v);
            }
            None => {
                cmd.env_remove(k);
            }
        }
    }
}

fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn cloexec(fd: RawFd) -> RawFd {
    unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    fd
}

/// zend's waitpid_cached: a WIFEXITED status is latched on the proc
/// resource and replayed forever after; stopped/signaled transitions
/// are re-polled each call.
fn waitpid_cached(pid: i32, options: i32, cached: &mut Option<i32>) -> (i32, i32) {
    if let Some(st) = *cached {
        return (pid, st);
    }
    let mut st = 0;
    let r = unsafe { libc::waitpid(pid, &mut st, options) };
    if r > 0 && libc::WIFEXITED(st) {
        *cached = Some(st);
    }
    (r, st)
}

fn proc_handle(args: &[Cell], fname: &str) -> Result<Rc<RefCell<PhpResource>>, PhpError> {
    match arg(args, 0) {
        Value::Resource(r) => Ok(r),
        v => err(
            "TypeError",
            format!(
                "{}(): Argument #1 ($process) must be of type resource, {} given",
                fname,
                zval_word(&v)
            ),
        ),
    }
}

fn proc_close(_it: &mut Interp, args: &[Cell]) -> Result<Value, PhpError> {
    let r = proc_handle(args, "proc_close")?;
    let mut rb = r.borrow_mut();
    let PhpResource::Proc {
        pid,
        cached_status,
        closed,
        pipes,
        ..
    } = &mut *rb
    else {
        return err(
            "TypeError",
            "proc_close(): supplied resource is not a valid process resource",
        );
    };
    if *closed {
        return err(
            "TypeError",
            "proc_close(): supplied resource is not a valid process resource",
        );
    }
    *closed = true;
    let mut pipes_taken = Vec::new();
    std::mem::swap(&mut pipes_taken, pipes);
    for p in pipes_taken {
        let mut b = p.borrow_mut();
        if let PhpResource::Pipe { id, .. } = &*b {
            *b = PhpResource::Closed { id: *id };
        }
    }
    let ret = loop {
        let (wp, st) = waitpid_cached(*pid, 0, cached_status);
        if wp == -1 && errno() == libc::EINTR {
            continue;
        }
        break if wp <= 0 {
            -1
        } else if libc::WIFEXITED(st) {
            libc::WEXITSTATUS(st) as i64
        } else {
            st as i64
        };
    };
    let id = rb.id();
    *rb = PhpResource::Closed { id };
    Ok(Value::Int(ret))
}

fn proc_get_status(_it: &mut Interp, args: &[Cell]) -> Result<Value, PhpError> {
    let r = proc_handle(args, "proc_get_status")?;
    let mut rb = r.borrow_mut();
    let PhpResource::Proc {
        pid,
        command,
        cached_status,
        closed,
        ..
    } = &mut *rb
    else {
        return err(
            "TypeError",
            "proc_get_status(): supplied resource is not a valid process resource",
        );
    };
    if *closed {
        return err(
            "TypeError",
            "proc_get_status(): supplied resource is not a valid process resource",
        );
    }
    let (wp, wstatus) = waitpid_cached(*pid, libc::WNOHANG | libc::WUNTRACED, cached_status);
    let mut running = true;
    let (mut signaled, mut stopped) = (false, false);
    let (mut exitcode, mut termsig, mut stopsig) = (-1i64, 0i64, 0i64);
    if wp == *pid {
        if libc::WIFEXITED(wstatus) {
            running = false;
            exitcode = libc::WEXITSTATUS(wstatus) as i64;
        }
        if libc::WIFSIGNALED(wstatus) {
            running = false;
            signaled = true;
            termsig = libc::WTERMSIG(wstatus) as i64;
        }
        if libc::WIFSTOPPED(wstatus) {
            stopped = true;
            stopsig = libc::WSTOPSIG(wstatus) as i64;
        }
    } else if wp == -1 {
        running = false;
    }
    let mut a = PhpArray::new();
    for (k, v) in [
        ("command", Value::str(command.clone())),
        ("pid", Value::Int(*pid as i64)),
        ("cached", Value::Bool(cached_status.is_some())),
        ("running", Value::Bool(running)),
        ("signaled", Value::Bool(signaled)),
        ("stopped", Value::Bool(stopped)),
        ("exitcode", Value::Int(exitcode)),
        ("termsig", Value::Int(termsig)),
        ("stopsig", Value::Int(stopsig)),
    ] {
        a.set(ArrKey::Str(k.into()), v);
    }
    Ok(Value::Array(Rc::new(RefCell::new(a))))
}

fn proc_terminate(it: &mut Interp, args: &[Cell]) -> Result<Value, PhpError> {
    let r = proc_handle(args, "proc_terminate")?;
    // zend zpp: arg1 resource type, then arg2 signal int type, and only
    // then the valid-process-resource check.
    let sig = match args.get(1) {
        Some(_) => zpp_long_arg(it, args, 1, "proc_terminate", 2, "$signal")? as i32,
        None => libc::SIGTERM,
    };
    let rb = r.borrow();
    let PhpResource::Proc { pid, closed, .. } = &*rb else {
        return err(
            "TypeError",
            "proc_terminate(): supplied resource is not a valid process resource",
        );
    };
    if *closed {
        return err(
            "TypeError",
            "proc_terminate(): supplied resource is not a valid process resource",
        );
    }
    Ok(Value::Bool(unsafe { libc::kill(*pid, sig) } == 0))
}

/// One built descriptorspec entry: `child` is installed at `index` in
/// the child (dup2), `parent` survives in the parent (closed in the
/// child) and becomes a $pipes stream when `pipe_rw` is Some.
#[derive(Clone, Copy)]
struct Desc {
    index: i32,
    child: RawFd,
    parent: RawFd,
    pipe_rw: Option<bool>,
    socket: bool,
    /// ["pty"] descriptor — the parent's end is the pty master (r+).
    pty: bool,
}

fn dup_fd(from: RawFd) -> Option<RawFd> {
    let r = unsafe { libc::dup(from) };
    if r < 0 {
        None
    } else {
        Some(r)
    }
}

fn spec_err(it: &mut Interp, msg: &str) -> Result<(), PhpError> {
    it.warn_pub(&format!("proc_open(): {}", msg))
}

fn resource_child_fd(
    it: &mut Interp,
    r: &PhpResource,
    index: i64,
) -> Result<Option<RawFd>, PhpError> {
    let fd = match r {
        PhpResource::File { file, .. } | PhpResource::Pipe { file, .. } => file.as_raw_fd(),
        PhpResource::Stdio { which, .. } => *which as RawFd,
        PhpResource::Proc { .. } | PhpResource::Closed { .. } => {
            return err(
                "TypeError",
                "proc_open(): supplied resource is not a valid stream resource",
            )
        }
        PhpResource::Mem { .. } => {
            spec_err(
                it,
                "Cannot represent a stream of type MEMORY as a File Descriptor",
            )?;
            return Ok(None);
        }
        PhpResource::Input { .. } => {
            spec_err(
                it,
                "Cannot represent a stream of type Input as a File Descriptor",
            )?;
            return Ok(None);
        }
        PhpResource::Other { kind, .. } => {
            spec_err(
                it,
                &format!(
                    "Cannot represent a stream of type {} as a File Descriptor",
                    kind
                ),
            )?;
            return Ok(None);
        }
    };
    match dup_fd(fd) {
        Some(d) => Ok(Some(d)),
        None => {
            spec_err(
                it,
                &format!(
                    "Failed to dup() for descriptor {}: {}",
                    index,
                    io_err_str(&std::io::Error::last_os_error())
                ),
            )?;
            Ok(None)
        }
    }
}

fn proc_open(it: &mut Interp, args: &[Cell]) -> Result<Value, PhpError> {
    // command: array → argv exec, anything else → /bin/sh -c string.
    let cmd_v = arg(args, 0);
    let (argv, command_str, cmd_bytes): (Option<Vec<Vec<u8>>>, String, Vec<u8>) = match &cmd_v {
        Value::Array(a) => {
            let elems: Vec<Vec<u8>> = a
                .borrow()
                .iter()
                .map(|(_, c)| it.to_bytes_of(&c.borrow()))
                .collect();
            if elems.is_empty() {
                return err(
                    "ValueError",
                    "proc_open(): Argument #1 ($command) must not be empty",
                );
            }
            // zend rejects an empty argv[0] before the NUL scan.
            if elems[0].is_empty() {
                return err(
                    "ValueError",
                    "First element must contain a non-empty program name",
                );
            }
            // zend checks every element for NUL before exec and throws
            // "Command array element N contains a null byte" (1-based).
            for (i, e) in elems.iter().enumerate() {
                if e.contains(&0) {
                    return err(
                        "ValueError",
                        format!("Command array element {} contains a null byte", i + 1),
                    );
                }
            }
            (
                Some(elems.clone()),
                String::from_utf8_lossy(&elems[0]).into_owned(),
                Vec::new(),
            )
        }
        v => {
            // An empty string is legal — zend runs /bin/sh -c ''
            // (the shell exits 0, proc_close reports 0).
            let b = it.to_bytes_of(v);
            (None, String::from_utf8_lossy(&b).into_owned(), b)
        }
    };

    let spec = match arg(args, 1) {
        Value::Array(a) => a,
        v => {
            return err(
                "TypeError",
                format!(
                    "proc_open(): Argument #2 ($descriptor_spec) must be of type array, {} given",
                    zval_word(&v)
                ),
            )
        }
    };
    let spec_items: Vec<(ArrKey, Value)> = spec
        .borrow()
        .iter()
        .map(|(k, c)| (k.clone(), c.borrow().clone()))
        .collect();

    // cwd / env_vars / options types are checked before any descriptor
    // work, like zend's zpp.
    let cwd = match &arg(args, 3) {
        Value::Null => None,
        v @ (Value::Array(_) | Value::Resource(_)) => {
            return err(
                "TypeError",
                format!(
                    "proc_open(): Argument #4 ($cwd) must be of type ?string, {} given",
                    zval_word(v)
                ),
            )
        }
        v => Some(it.to_bytes_of(v)),
    };
    let env = match &arg(args, 4) {
        Value::Null => None,
        Value::Array(a) => Some(a.clone()),
        v => {
            return err(
                "TypeError",
                format!(
                    "proc_open(): Argument #5 ($env_vars) must be of type ?array, {} given",
                    zval_word(v)
                ),
            )
        }
    };
    match &arg(args, 5) {
        Value::Null | Value::Array(_) => {}
        v => {
            return err(
                "TypeError",
                format!(
                    "proc_open(): Argument #6 ($options) must be of type ?array, {} given",
                    zval_word(v)
                ),
            )
        }
    }

    let mut descs: Vec<Desc> = Vec::new();
    let mut pty_pair: Option<(RawFd, RawFd)> = None;
    // Error paths must release the fds already allocated for earlier
    // spec entries (and any pty pair) — `?` would leak them.
    macro_rules! cleanup {
        () => {{
            if let Some((m, s)) = pty_pair {
                unsafe {
                    libc::close(m);
                    libc::close(s);
                }
            }
            close_descs(&mut descs);
        }};
    }
    for (k, v) in &spec_items {
        let index = match k {
            ArrKey::Int(i) => *i as i32,
            _ => {
                cleanup!();
                return err(
                    "ValueError",
                    "proc_open(): Argument #2 ($descriptor_spec) must be an integer indexed array",
                );
            }
        };
        let res = match v {
            Value::Resource(r) => {
                let rb = r.borrow();
                match resource_child_fd(it, &rb, index as i64) {
                    Ok(f) => f.map(|fd| Desc {
                        index,
                        child: fd,
                        parent: -1,
                        pipe_rw: None,
                        socket: false,
                        pty: false,
                    }),
                    Err(e) => {
                        drop(rb);
                        cleanup!();
                        return Err(e);
                    }
                }
            }
            Value::Array(a) => match spec_array(it, a.clone(), index, &descs, &mut pty_pair) {
                Ok(d) => d,
                Err(e) => {
                    cleanup!();
                    return Err(e);
                }
            },
            _ => {
                cleanup!();
                return err(
                    "ValueError",
                    "proc_open(): Argument #2 ($descriptor_spec) must only contain arrays and streams",
                );
            }
        };
        match res {
            Some(d) => descs.push(d),
            None => {
                cleanup!();
                return Ok(Value::Bool(false));
            }
        }
    }
    if let Some((m, s)) = pty_pair {
        unsafe {
            libc::close(m);
            libc::close(s);
        }
    }

    let mut cmd = match &argv {
        Some(av) => {
            let mut c = Command::new(to_os(&resolve_argv0(&av[0])));
            for a in &av[1..] {
                c.arg(to_os(trunc_nul(a)));
            }
            c
        }
        None => {
            let mut c = Command::new("/bin/sh");
            // zend execle()s /bin/sh with argv[0]="sh" — dash prints
            // that name ("sh: 1: x: not found") in stderr.
            c.arg0("sh");
            c.arg("-c").arg(to_os(trunc_nul(&cmd_bytes)));
            c
        }
    };

    if let Some(env_arr) = &env {
        cmd.env_clear();
        let items: Vec<(ArrKey, Vec<u8>)> = env_arr
            .borrow()
            .iter()
            .map(|(k, c)| (k.clone(), it.to_bytes_of(&c.borrow())))
            .collect();
        for (k, v) in items {
            // zend builds the envp entry as a C string "K=v": an empty
            // value produces nothing, but a NUL inside truncates at the
            // byte ("\0x" still lands as "K=" → empty, set).
            if v.is_empty() {
                continue;
            }
            let v = match v.iter().position(|&b| b == 0) {
                Some(p) => &v[..p],
                None => &v[..],
            };
            match k {
                ArrKey::Str(ks) if !ks.is_empty() && !ks.contains('\0') => {
                    // Keys containing '=' are injected verbatim by zend's
                    // putenv-style envp assembly: 'A=B' => 'v' lands as
                    // "A=B=v", which resolves to name A / value B=v.
                    match ks.find('=') {
                        Some(p) if p > 0 => {
                            let mut joined = ks.as_bytes()[p + 1..].to_vec();
                            joined.push(b'=');
                            joined.extend_from_slice(v);
                            cmd.env(to_os(ks[..p].as_bytes()), to_os(&joined));
                        }
                        Some(_) => {}
                        None => {
                            cmd.env(to_os(ks.as_bytes()), to_os(v));
                        }
                    }
                }
                // Numeric-keyed entries are injected bare by zend
                // ("v" or "K=V" text as-is); entries without '=' are
                // dead weight in environ — env(1)/getenv can't see
                // them — so skipping is behaviorally identical.
                _ => {
                    if let Some(p) = v.iter().position(|&b| b == b'=') {
                        if !v[..p].is_empty() {
                            cmd.env(to_os(&v[..p]), to_os(&v[p + 1..]));
                        }
                    }
                }
            }
        }
    } else {
        // $env = null → inherit (overridden by putenv() state).
        apply_env_overrides(it, &mut cmd);
    }
    if let Some(dir) = &cwd {
        cmd.current_dir(to_os(trunc_nul(dir)));
    }

    {
        // Install child ends at their descriptor indexes inside the
        // spawned process: close parent ends, dup2(child, index), then
        // close the original child fd — zend's child_init path.
        let descs_c = descs.clone();
        unsafe {
            cmd.pre_exec(move || {
                // Rust's Command resets SIGPIPE to SIG_DFL before exec;
                // zend children inherit PHP's SIG_IGN so writes to dead
                // pipes fail EPIPE instead of killing the child.
                libc::signal(libc::SIGPIPE, libc::SIG_IGN);
                for d in descs_c.iter() {
                    if d.parent >= 0 {
                        libc::close(d.parent);
                    }
                    if libc::dup2(d.child, d.index) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if d.child != d.index {
                        libc::close(d.child);
                    }
                }
                Ok(())
            });
        }
    }

    let mut pipes_arr = PhpArray::new();
    match cmd.spawn() {
        Err(e) => {
            for d in &descs {
                unsafe {
                    libc::close(d.child);
                    if d.parent >= 0 {
                        libc::close(d.parent);
                    }
                }
            }
            it.warn_pub(&format!(
                "proc_open(): posix_spawn() failed: {}",
                io_err_str(&e)
            ))?;
            Ok(Value::Bool(false))
        }
        Ok(child) => {
            let pid = child.id() as i32;
            drop(child);
            let mut proc_pipes = Vec::new();
            for d in &descs {
                unsafe {
                    libc::close(d.child);
                }
                if let Some(wr) = d.pipe_rw {
                    let f = unsafe { std::fs::File::from_raw_fd(d.parent) };
                    let res = Rc::new(RefCell::new(PhpResource::Pipe {
                        id: it.next_res_id(),
                        file: f,
                        write: wr,
                        socket: d.socket,
                        pty: d.pty,
                        nonblock: false,
                        pos: 0,
                        eof: false,
                    }));
                    proc_pipes.push(res.clone());
                    pipes_arr.set(ArrKey::Int(d.index as i64), Value::Resource(res));
                } else if d.parent >= 0 {
                    unsafe {
                        libc::close(d.parent);
                    }
                }
            }
            if let Some(pc) = args.get(2) {
                *pc.borrow_mut() = Value::Array(Rc::new(RefCell::new(pipes_arr)));
            }
            Ok(Value::Resource(Rc::new(RefCell::new(PhpResource::Proc {
                id: it.next_res_id(),
                pid,
                command: command_str,
                cached_status: None,
                closed: false,
                pipes: proc_pipes,
            }))))
        }
    }
}

fn close_descs(descs: &mut Vec<Desc>) {
    for d in descs.drain(..) {
        unsafe {
            libc::close(d.child);
            if d.parent >= 0 {
                libc::close(d.parent);
            }
        }
    }
}

fn spec_array(
    it: &mut Interp,
    a: Rc<RefCell<PhpArray>>,
    index: i32,
    descs: &[Desc],
    pty_pair: &mut Option<(RawFd, RawFd)>,
) -> Result<Option<Desc>, PhpError> {
    let items: Vec<Value> = a.borrow().iter().map(|(_, c)| c.borrow().clone()).collect();
    let get_bytes = |it: &mut Interp, i: usize, name: &str| -> Result<Vec<u8>, PhpError> {
        match items.get(i) {
            Some(v) => Ok(it.to_bytes_of(v)),
            None => err("ValueError", format!("Missing {}", name)),
        }
    };
    let ty = get_bytes(it, 0, "handle qualifier")?;
    let d = match ty.as_slice() {
        b"pipe" => {
            let mode = get_bytes(it, 1, "mode parameter for 'pipe'")?;
            let mut p = [0i32; 2];
            if unsafe { libc::pipe(p.as_mut_ptr()) } != 0 {
                spec_err(
                    it,
                    &format!(
                        "Unable to create pipe {}",
                        io_err_str(&std::io::Error::last_os_error())
                    ),
                )?;
                return Ok(None);
            }
            let (child, parent, rw) = if mode.first() == Some(&b'w') {
                (p[1], p[0], false)
            } else {
                (p[0], p[1], true)
            };
            Some(Desc {
                index,
                child,
                parent: cloexec(parent),
                pipe_rw: Some(rw),
                socket: false,
                pty: false,
            })
        }
        b"socket" => {
            let mut s = [0i32; 2];
            if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, s.as_mut_ptr()) } != 0
            {
                spec_err(
                    it,
                    &format!(
                        "Unable to create socket pair: {}",
                        io_err_str(&std::io::Error::last_os_error())
                    ),
                )?;
                return Ok(None);
            }
            Some(Desc {
                index,
                child: s[1],
                parent: cloexec(s[0]),
                pipe_rw: Some(true),
                socket: true,
                pty: false,
            })
        }
        b"file" => {
            let path = get_bytes(it, 1, "file name parameter for 'file'")?;
            let mode = get_bytes(it, 2, "mode parameter for 'file'")?;
            let path_s = String::from_utf8_lossy(&path).into_owned();
            let mode_s = String::from_utf8_lossy(&mode).into_owned();
            let first = mode_s.chars().next().unwrap_or('\0');
            if !matches!(first, 'r' | 'w' | 'a' | 'x' | 'c')
                || mode_s
                    .chars()
                    .skip(1)
                    .any(|c| !matches!(c, '+' | 'b' | 't' | 'e'))
            {
                it.warn_pub(&format!(
                    "proc_open({}): Failed to open stream: `{}' is not a valid mode for fopen",
                    path_s, mode_s
                ))?;
                return Ok(None);
            }
            match crate::builtins::fs::fopen(&path_s, &mode_s) {
                Ok(f) => Some(Desc {
                    index,
                    child: f.into_raw_fd(),
                    parent: -1,
                    pipe_rw: None,
                    socket: false,
                    pty: false,
                }),
                Err(e) => {
                    it.warn_pub(&format!(
                        "proc_open({}): Failed to open stream: {}",
                        path_s,
                        io_err_str(&e)
                    ))?;
                    return Ok(None);
                }
            }
        }
        b"redirect" => {
            let target = match items.get(1) {
                None => return err("ValueError", "Missing redirection target"),
                Some(Value::Int(i)) => *i as i32,
                Some(v) => {
                    return err(
                        "ValueError",
                        format!(
                            "Redirection target must be of type int, {} given",
                            zval_word(v)
                        ),
                    )
                }
            };
            let from = descs
                .iter()
                .find(|d| d.index == target)
                .map(|d| d.child)
                .or(if (0..=2).contains(&target) {
                    Some(target)
                } else {
                    None
                });
            match from {
                Some(fd) => match dup_fd(fd) {
                    Some(d2) => Some(Desc {
                        index,
                        child: d2,
                        parent: -1,
                        pipe_rw: None,
                        socket: false,
                        pty: false,
                    }),
                    None => {
                        spec_err(
                            it,
                            &format!(
                                "Failed to dup() for descriptor {}: {}",
                                index,
                                io_err_str(&std::io::Error::last_os_error())
                            ),
                        )?;
                        None
                    }
                },
                None => {
                    spec_err(it, &format!("Redirection target {} not found", target))?;
                    None
                }
            }
        }
        b"null" => {
            let r = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/null");
            match r {
                Ok(f) => Some(Desc {
                    index,
                    child: f.into_raw_fd(),
                    parent: -1,
                    pipe_rw: None,
                    socket: false,
                    pty: false,
                }),
                Err(e) => {
                    spec_err(it, &format!("Failed to open /dev/null: {}", io_err_str(&e)))?;
                    return Ok(None);
                }
            }
        }
        b"pty" => {
            if pty_pair.is_none() {
                let (mut m, mut s) = (-1, -1);
                if unsafe {
                    libc::openpty(
                        &mut m,
                        &mut s,
                        std::ptr::null_mut(),
                        std::ptr::null(),
                        std::ptr::null(),
                    )
                } != 0
                {
                    spec_err(
                        it,
                        &format!(
                            "Could not open PTY (pseudoterminal): {}",
                            io_err_str(&std::io::Error::last_os_error())
                        ),
                    )?;
                    return Ok(None);
                }
                cloexec(m);
                cloexec(s);
                *pty_pair = Some((m, s));
            }
            let (m, s) = pty_pair.unwrap();
            match (dup_fd(s), dup_fd(m)) {
                (Some(cd), Some(pd)) => Some(Desc {
                    index,
                    child: cd,
                    parent: pd,
                    pipe_rw: Some(true),
                    socket: false,
                    pty: true,
                }),
                (cd, pd) => {
                    for f in [cd, pd].into_iter().flatten() {
                        unsafe {
                            libc::close(f);
                        }
                    }
                    spec_err(
                        it,
                        &format!(
                            "Failed to dup() for descriptor {}: {}",
                            index,
                            io_err_str(&std::io::Error::last_os_error())
                        ),
                    )?;
                    None
                }
            }
        }
        other => {
            spec_err(
                it,
                &format!(
                    "{} is not a valid descriptor spec/mode",
                    String::from_utf8_lossy(other)
                ),
            )?;
            None
        }
    };
    Ok(d)
}

/// php_exec(): spawn `/bin/sh -c cmd`, read stdout to EOF, then apply
/// the per-type output contract. `ty`: 0/2 exec (2 appends lines to
/// $output), 1 system (echoes), 3 passthru (raw bytes out, null ret).
fn php_exec(it: &mut Interp, fname: &str, args: &[Cell], ty: u8) -> Result<Value, PhpError> {
    let cmd = arg_bs(it, args, 0);
    if cmd.is_empty() {
        return err(
            "ValueError",
            format!("{}(): Argument #1 ($command) must not be empty", fname),
        );
    }
    if cmd.contains(&0) {
        return err(
            "ValueError",
            format!(
                "{}(): Argument #1 ($command) must not contain any null bytes",
                fname
            ),
        );
    }
    let mut c = Command::new("/bin/sh");
    c.arg0("sh")
        .arg("-c")
        .arg(to_os(&cmd))
        .stdout(Stdio::piped());
    apply_env_overrides(it, &mut c);
    let mut child = match c.spawn() {
        Ok(c) => c,
        Err(_) => {
            if ty == 3 {
                it.warn_pub(&format!(
                    "Unable to execute '{}'",
                    String::from_utf8_lossy(&cmd)
                ))?;
            } else {
                it.warn_pub(&format!(
                    "Unable to fork [{}]",
                    String::from_utf8_lossy(&cmd)
                ))?;
            }
            return Ok(Value::Bool(false));
        }
    };
    let mut out = Vec::new();
    if let Some(mut so) = child.stdout.take() {
        let _ = so.read_to_end(&mut out);
    }
    let raw = child
        .wait()
        .map(|s| {
            use std::os::unix::process::ExitStatusExt;
            s.into_raw()
        })
        .unwrap_or(-1);
    let code = if raw >= 0 && libc::WIFEXITED(raw) {
        libc::WEXITSTATUS(raw) as i64
    } else {
        raw as i64
    };
    // $result_code sits at #2 for exec, #1 for system/passthru.
    if let Some(c) = args.get(if fname == "exec" { 2 } else { 1 }) {
        *c.borrow_mut() = Value::Int(code);
    }

    // zend builds the return line and appends array entries per
    // whitespace-stripped logical line (unbounded); system/passthru
    // push raw bytes to output as read.
    if ty == 1 || ty == 3 {
        it.emit_bytes(&out);
    }
    if ty == 3 {
        return Ok(Value::Null);
    }
    let lines: Vec<&[u8]> = out.split_inclusive(|&b| b == b'\n').collect();
    if ty == 2 {
        // $output: separate the COW array like zend's SEPARATE_ARRAY,
        // then append each stripped line.
        if let Some(c) = args.get(1) {
            let arr = {
                let mut cb = c.borrow_mut();
                match &*cb {
                    Value::Array(a) => {
                        if Rc::strong_count(a) > 1 {
                            let na = PhpArray {
                                entries: a.borrow().entries.clone(),
                                next: a.borrow().next,
                                is_ref: a.borrow().is_ref,
                                iter_pos: a.borrow().iter_pos,
                            };
                            let nv = Rc::new(RefCell::new(na));
                            *cb = Value::Array(nv.clone());
                            nv
                        } else {
                            a.clone()
                        }
                    }
                    _ => {
                        let nv = Rc::new(RefCell::new(PhpArray::new()));
                        *cb = Value::Array(nv.clone());
                        nv
                    }
                }
            };
            for l in &lines {
                arr.borrow_mut().push(Value::bytes(strip_ws(l).to_vec()));
            }
        }
    }
    let last: &[u8] = lines.last().copied().unwrap_or(b"");
    Ok(Value::bytes(strip_ws(last).to_vec()))
}

/// C isspace() for the byte domain — zend's strip_trailing_whitespace.
fn strip_ws(b: &[u8]) -> &[u8] {
    let mut n = b.len();
    while n > 0 && matches!(b[n - 1], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
        n -= 1;
    }
    &b[..n]
}

/// shell_exec(): full stdout as one string; null on empty output or
/// failed exec (zend's shell_exec returns false on popen failure, and
/// PHP 8 keeps that — RETVAL_FALSE).
fn shell_exec(it: &mut Interp, args: &[Cell]) -> Result<Value, PhpError> {
    let cmd = arg_bs(it, args, 0);
    if cmd.is_empty() {
        return err(
            "ValueError",
            "shell_exec(): Argument #1 ($command) must not be empty",
        );
    }
    if cmd.contains(&0) {
        return err(
            "ValueError",
            "shell_exec(): Argument #1 ($command) must not contain any null bytes",
        );
    }
    let mut c = Command::new("/bin/sh");
    c.arg0("sh")
        .arg("-c")
        .arg(to_os(&cmd))
        .stdout(Stdio::piped());
    apply_env_overrides(it, &mut c);
    match c.spawn() {
        Ok(mut c) => {
            let mut out = Vec::new();
            if let Some(mut so) = c.stdout.take() {
                let _ = so.read_to_end(&mut out);
            }
            let _ = c.wait();
            Ok(if out.is_empty() {
                Value::Null
            } else {
                Value::bytes(out)
            })
        }
        Err(_) => {
            it.warn_pub(&format!(
                "Unable to execute '{}'",
                String::from_utf8_lossy(&cmd)
            ))?;
            Ok(Value::Bool(false))
        }
    }
}

/// php_mblen for UTF-8: byte length of a valid multibyte sequence at
/// `i`, 1 for ASCII, -1 for an invalid leading/continuation byte.
fn mb_len(b: &[u8], i: usize) -> i32 {
    let c = b[i];
    if c < 0x80 {
        return 1;
    }
    // glibc mbrlen follows the RFC-2279 (original UTF-8) lead-byte
    // table: F1-F7 are 4-byte leads, F8-FB 5-byte, FC-FD 6-byte — the
    // upper bounds on each class are enforced via the second byte
    // (F0 needs >=0x90, F8-FB >=0x88, FC/FD >=0x84) like the E0/ED
    // special cases below.
    let n = match c {
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        0xF8..=0xFB => 5,
        0xFC..=0xFD => 6,
        _ => return -1,
    };
    if i + n > b.len() || !b[i + 1..i + n].iter().all(|&x| x & 0xC0 == 0x80) {
        return -1;
    }
    // glibc mbrlen rejects overlong encodings and UTF-16 surrogates
    // via the second byte, only on the FIRST lead of each class
    // (E0 → >=0xA0, ED → <=0x9F, F0 → >=0x90, F8 → >=0x88, FC → >=0x84);
    // the rest of each class (F1-F7, F9-FB, FD) is unbounded, so
    // >U+10FFFF sequences are kept.
    match (c, b[i + 1]) {
        (0xE0, s) if s < 0xA0 => return -1,
        (0xED, s) if s > 0x9F => return -1,
        (0xF0, s) if s < 0x90 => return -1,
        (0xF8, s) if s < 0x88 => return -1,
        (0xFC, s) if s < 0x84 => return -1,
        _ => {}
    }
    n as i32
}

/// escapeshellarg(): 'arg' with each ' replaced by '\'' — plus zend's
/// invalid-UTF8 byte dropping.
fn escape_shell_arg(it: &mut Interp, args: &[Cell]) -> Result<Value, PhpError> {
    let s = arg_bs(it, args, 0);
    let mut out = Vec::with_capacity(s.len() + 2);
    out.push(b'\'');
    let mut x = 0;
    while x < s.len() {
        let ml = mb_len(&s, x);
        if ml < 0 {
            x += 1;
            continue;
        }
        if ml > 1 {
            out.extend_from_slice(&s[x..x + ml as usize]);
            x += ml as usize;
            continue;
        }
        if s[x] == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(s[x]);
        }
        x += 1;
    }
    out.push(b'\'');
    Ok(Value::bytes(out))
}

/// escapeshellcmd(): backslash-escape the metachar table, with zend's
/// paired-quote logic and invalid-UTF8 dropping.
fn escape_shell_cmd(it: &mut Interp, args: &[Cell]) -> Result<Value, PhpError> {
    let s = arg_bs(it, args, 0);
    let l = s.len();
    let mut out = Vec::with_capacity(l + 8);
    let mut pair: Option<u8> = None; // quote byte zend's `p` is tracking
    let mut x = 0;
    while x < l {
        let ml = mb_len(&s, x);
        if ml < 0 {
            x += 1;
            continue;
        }
        if ml > 1 {
            out.extend_from_slice(&s[x..x + ml as usize]);
            x += ml as usize;
            continue;
        }
        let c = s[x];
        match c {
            b'"' | b'\'' => {
                match pair {
                    // Unpaired quote → backslash; a later same quote
                    // starts a tracked pair (zend's `p` pointer).
                    None => {
                        if s[x + 1..].contains(&c) {
                            pair = Some(c);
                        } else {
                            out.push(b'\\');
                        }
                    }
                    Some(q) if q == c => pair = None,
                    Some(_) => out.push(b'\\'),
                }
                out.push(c);
            }
            b'#' | b'&' | b';' | b'`' | b'|' | b'*' | b'?' | b'~' | b'<' | b'>' | b'^' | b'('
            | b')' | b'[' | b']' | b'{' | b'}' | b'$' | b'\\' | 0x0A | 0xFF => {
                out.push(b'\\');
                out.push(c);
            }
            _ => out.push(c),
        }
        x += 1;
    }
    Ok(Value::bytes(out))
}
