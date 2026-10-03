//! `phpun serve`: minimal HTTP/1.1 dev server.
//!
//! Request → fresh interpreter → response: the classic PHP shared-nothing
//! model, so request isolation bugs can't leak between hits. A future
//! persistent-worker mode swaps the per-request `Interp::new` factory for
//! a hot runtime without touching this socket/parsing layer.

use crate::builtins::urldecode;
use crate::interp::Interp;
use crate::value::{ArrKey, PhpArray, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const HEADER_CAP: usize = 64 * 1024;
const BODY_CAP: usize = 16 * 1024 * 1024;

/// `phpun serve <file> --host H --port N [--docroot DIR] [--worker]`. Blocks
/// until the process dies. `docroot` enables front-controller mode: existing
/// files under it are served directly (`.php` ones run as scripts, the rest
/// as static content); everything else routes to `file`. Default docroot is
/// the front script's directory.
///
/// `workers` > 0 switches to persistent-worker mode: each worker boots the
/// script ONCE on its own warm interpreter; the script's top-level `return`
/// must be a callable invoked per request as `handler(array $req)`. Worker
/// state is per-worker — a global incremented in the handler persists
/// across requests on that worker only. `workers == 0` keeps the classic
/// fresh-interp-per-request model.
pub fn serve(file: &str, host: &str, port: u16, docroot: Option<&str>, workers: usize) -> i32 {
    let canon = std::fs::canonicalize(file)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| file.to_string());
    let dr = match docroot {
        Some(d) => std::fs::canonicalize(d)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| d.to_string()),
        None => std::path::Path::new(&canon)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| ".".to_string()),
    };
    let listener = match TcpListener::bind((host, port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("phpun serve: cannot bind {host}:{port}: {e}");
            return 1;
        }
    };
    let mode = if workers > 0 {
        format!("{} worker(s)", workers)
    } else {
        "fresh-interp".to_string()
    };
    eprintln!("phpun serve: http://{host}:{port} → {canon} (docroot {dr}, {mode})");
    let cfg = std::sync::Arc::new(Cfg {
        front: canon,
        docroot: dr,
        host: host.to_string(),
        port,
    });
    if workers > 0 {
        let mut senders = Vec::new();
        for w in 0..workers {
            let (tx, rx) = std::sync::mpsc::channel::<TcpStream>();
            let c = cfg.clone();
            std::thread::spawn(move || worker_loop(w, rx, c));
            senders.push(tx);
        }
        let mut n = 0usize;
        for conn in listener.incoming() {
            match conn {
                Ok(stream) => {
                    let _ = senders[n % senders.len()].send(stream);
                    n = n.wrapping_add(1);
                }
                Err(e) => eprintln!("phpun serve: accept: {e}"),
            }
        }
        return 0;
    }
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let c = cfg.clone();
                std::thread::spawn(move || handle(stream, c));
            }
            Err(e) => eprintln!("phpun serve: accept: {e}"),
        }
    }
    0
}

/// One warm interpreter per worker. The script boots once; its top-level
/// `return`ed callable handles every request this worker receives.
fn worker_loop(id: usize, rx: std::sync::mpsc::Receiver<TcpStream>, cfg: std::sync::Arc<Cfg>) {
    let src = match std::fs::read_to_string(&cfg.front) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("phpun serve: worker {id}: cannot read {}: {e}", cfg.front);
            return;
        }
    };
    let mut it = Interp::new(&cfg.front);
    let (res, ret) = it.run_source_ret(&src);
    if !it.out.is_empty() {
        let _ = std::io::Write::write_all(&mut std::io::stderr(), &it.out);
    }
    if !it.err_buf.is_empty() {
        eprint!("{}", it.err_buf);
    }
    if res.fatal.is_some() {
        eprintln!("phpun serve: worker {id}: boot failed — exiting");
        return;
    }
    let handler = match ret {
        Some(Value::Callable(_)) | Some(Value::Str(_)) => ret.unwrap(),
        _ => {
            eprintln!(
                "phpun serve: worker {id}: {} did not return a callable; \
                 running requests on fresh interps",
                cfg.front
            );
            for stream in rx {
                handle(stream, cfg.clone());
            }
            return;
        }
    };
    it.seal_boot_objects();
    for stream in rx {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
        let remote = stream
            .peer_addr()
            .map(|a| (a.ip().to_string(), a.port()))
            .unwrap_or_else(|_| (String::new(), 0));
        let mut stream = stream;
        match read_request(&mut stream, remote) {
            Ok(Some(req)) => respond_worker(stream, &cfg, &req, &mut it, &handler),
            Ok(None) => {}
            Err(()) => {
                let body = "400 Bad Request\n";
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .as_bytes(),
                );
            }
        }
    }
}

/// Warm-worker request: reset per-request state, repopulate globals, then
/// invoke the boot-time handler with a request array
/// {method, uri, path, query, headers, body}.
fn respond_worker(mut stream: TcpStream, cfg: &Cfg, req: &Req, it: &mut Interp, handler: &Value) {
    // Static files and docroot .php scripts bypass the handler, same as
    // classic mode.
    match resolve_script(cfg, req) {
        Resolved::Static(p) => {
            let body = std::fs::read(&p).unwrap_or_default();
            let head = req.method == "HEAD";
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n",
                    mime_of(&p),
                    body.len()
                )
                .as_bytes(),
            );
            if !head {
                let _ = stream.write_all(&body);
            }
            return;
        }
        Resolved::Script(s) => {
            // A docroot .php runs on a throwaway interp — its state must
            // not leak into the warm world.
            respond(stream, cfg, req);
            let _ = s;
            return;
        }
        _ => {}
    }
    it.reset_request();
    populate(it, req, &cfg.front, cfg);

    let mut rarr = PhpArray::new();
    rarr.set(ArrKey::Str("method".into()), Value::str(req.method.clone()));
    rarr.set(ArrKey::Str("uri".into()), Value::str(req.uri.clone()));
    rarr.set(ArrKey::Str("path".into()), Value::str(req.path.clone()));
    rarr.set(ArrKey::Str("query".into()), Value::str(req.query.clone()));
    rarr.set(
        ArrKey::Str("body".into()),
        Value::str(String::from_utf8_lossy(&req.body).into_owned()),
    );
    let mut hdrs = PhpArray::new();
    for (k, v) in &req.headers {
        hdrs.set(ArrKey::Str(k.clone().into()), Value::str(v.clone()));
    }
    rarr.set(
        ArrKey::Str("headers".into()),
        Value::Array(std::rc::Rc::new(std::cell::RefCell::new(hdrs))),
    );

    let args =
        crate::interp::CallArgs::positional(vec![std::rc::Rc::new(std::cell::RefCell::new(
            Value::Array(std::rc::Rc::new(std::cell::RefCell::new(rarr))),
        ))]);
    let ret = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        it.call_value(handler, args)
    }))
    .unwrap_or_else(|_| Err(crate::error::PhpError::fatal("handler panicked", 0)));
    it.end_request();

    let mut code = it.resp_code;
    let mut lines: Vec<String> = it.out_headers.clone();
    let mut body = it.out.clone();
    if !it.err_buf.is_empty() {
        eprint!("{}", it.err_buf);
    }
    match ret {
        Ok(Value::Array(a)) => {
            let a = a.borrow();
            for (k, c) in a.iter() {
                let ArrKey::Str(name) = k else { continue };
                let v = c.borrow().clone();
                match name.as_ref() {
                    "status" => code = v.to_int(),
                    "headers" => {
                        if let Value::Array(h) = &v {
                            for (_, hc) in h.borrow().iter() {
                                lines.push(hc.borrow().to_php_string());
                            }
                        }
                    }
                    "body" => body.extend_from_slice(&v.to_php_bytes()),
                    _ => {}
                }
            }
        }
        Ok(Value::Str(s)) => body.extend_from_slice(&s),
        Ok(_) => {}
        Err(e) => {
            if code == 200 {
                code = 500;
            }
            body.extend_from_slice(format!("\n{}", e.message).as_bytes());
        }
    }
    if !lines
        .iter()
        .any(|h| h.to_lowercase().starts_with("content-type:"))
    {
        lines.push("Content-Type: text/html; charset=UTF-8".to_string());
    }
    let head_req = req.method == "HEAD";
    let mut resp = format!("HTTP/1.1 {} {}\r\n", code, reason(code));
    for h in &lines {
        resp.push_str(h);
        resp.push_str("\r\n");
    }
    resp.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    let mut resp = resp.into_bytes();
    if !head_req {
        resp.extend_from_slice(&body);
    }
    let _ = stream.write_all(&resp);
}

struct Cfg {
    front: String,
    docroot: String,
    host: String,
    port: u16,
}

struct Req {
    method: String,
    uri: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    remote_addr: String,
    remote_port: u16,
}

fn handle(mut stream: TcpStream, cfg: std::sync::Arc<Cfg>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let remote = stream
        .peer_addr()
        .map(|a| (a.ip().to_string(), a.port()))
        .unwrap_or_else(|_| (String::new(), 0));
    match read_request(&mut stream, remote) {
        Ok(Some(req)) => respond(stream, &cfg, &req),
        Ok(None) => {}
        Err(()) => {
            let body = "400 Bad Request\n";
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            );
        }
    }
}

/// Read one request: headers up to \r\n\r\n, then Content-Length bytes.
/// One request per connection (responses always close), which keeps the
/// state machine trivial — browsers just open another socket.
fn read_request(stream: &mut TcpStream, remote: (String, u16)) -> Result<Option<Req>, ()> {
    let mut raw = Vec::new();
    let mut tmp = [0u8; 8192];
    let head_end;
    loop {
        if let Some(pos) = find_seq(&raw, b"\r\n\r\n") {
            head_end = pos;
            break;
        }
        if raw.len() > HEADER_CAP {
            return Err(());
        }
        match stream.read(&mut tmp) {
            Ok(0) => return if raw.is_empty() { Ok(None) } else { Err(()) },
            Ok(n) => raw.extend_from_slice(&tmp[..n]),
            Err(_) => return Err(()),
        }
    }
    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let rl = lines.next().unwrap_or("");
    let mut parts = rl.split_whitespace();
    let method = parts.next().unwrap_or("").to_uppercase();
    let uri = parts.next().unwrap_or("/").to_string();
    if method.is_empty() || !rl.contains("HTTP/") {
        return Err(());
    }
    let (path, query) = match uri.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (uri.clone(), String::new()),
    };
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let clen: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0)
        .min(BODY_CAP);
    let mut body = raw[head_end + 4..].to_vec();
    while body.len() < clen {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&tmp[..n]),
            Err(_) => return Err(()),
        }
    }
    body.truncate(clen);
    Ok(Some(Req {
        method,
        uri,
        path: crate::value::lossy(&urldecode(path.as_bytes(), true)).into_owned(),
        query,
        headers,
        body,
        remote_addr: remote.0,
        remote_port: remote.1,
    }))
}

fn find_seq(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Map the request path under docroot: existing regular files are
/// served directly — `.php` ones run as scripts, everything else is a
/// static response. Missing paths route to the front script.
fn resolve_script<'a>(cfg: &'a Cfg, req: &Req) -> Resolved<'a> {
    let rel = req.path.trim_start_matches('/');
    if rel.is_empty() || rel.split('/').any(|s| s == "..") {
        return Resolved::Front;
    }
    let cand = std::path::Path::new(&cfg.docroot).join(rel);
    match std::fs::canonicalize(&cand) {
        Ok(p) if p.is_file() => {
            if p.extension().is_some_and(|e| e == "php") {
                Resolved::Script(p.display().to_string())
            } else {
                Resolved::Static(p)
            }
        }
        _ => Resolved::Front,
    }
}

enum Resolved<'a> {
    Front,
    Script(String),
    Static(std::path::PathBuf),
    #[allow(dead_code)]
    Phantom(&'a ()),
}

fn mime_of(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" | "map" => "application/json",
        "xml" => "text/xml",
        "txt" => "text/plain",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        _ => "application/octet-stream",
    }
}

fn respond(mut stream: TcpStream, cfg: &Cfg, req: &Req) {
    let script = match resolve_script(cfg, req) {
        Resolved::Static(p) => {
            let body = std::fs::read(&p).unwrap_or_default();
            let head = req.method == "HEAD";
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n",
                    mime_of(&p),
                    body.len()
                )
                .as_bytes(),
            );
            if !head {
                let _ = stream.write_all(&body);
            }
            return;
        }
        Resolved::Script(s) => s,
        Resolved::Front => cfg.front.clone(),
        Resolved::Phantom(_) => unreachable!(),
    };
    let file = script.as_str();
    let src = match std::fs::read_to_string(file) {
        Ok(s) => s,
        Err(e) => {
            let body = format!("phpun serve: cannot read {}: {}\n", file, e);
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Type: text/plain\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            );
            return;
        }
    };
    let mut it = Interp::new(file);
    populate(&mut it, req, file, cfg);
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| it.run_source(&src)))
        .unwrap_or_else(|_| crate::interp::RunResult {
            exit_code: 255,
            fatal: Some(crate::error::PhpError::fatal(
                "phpun: interpreter panicked during request",
                0,
            )),
        });

    let mut code = it.resp_code;
    if res.fatal.is_some() && code == 200 {
        code = 500;
    }
    let mut lines: Vec<String> = it.out_headers.clone();
    if !lines
        .iter()
        .any(|h| h.to_lowercase().starts_with("content-type:"))
    {
        lines.push("Content-Type: text/html; charset=UTF-8".to_string());
    }
    let head = req.method == "HEAD";
    let mut resp = format!("HTTP/1.1 {} {}\r\n", code, reason(code));
    for h in &lines {
        resp.push_str(h);
        resp.push_str("\r\n");
    }
    resp.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        it.out.len()
    ));
    let mut resp = resp.into_bytes();
    if !head {
        resp.extend_from_slice(&it.out);
    }
    let _ = stream.write_all(&resp);
}

/// Populate the request superglobals a PHP web script expects.
fn populate(it: &mut Interp, req: &Req, file: &str, cfg: &Cfg) {
    let set = |it: &mut Interp, k: &str, v: &str| it.set_server_var(k, v);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    set(it, "REQUEST_METHOD", &req.method);
    set(it, "REQUEST_URI", &req.uri);
    set(it, "QUERY_STRING", &req.query);
    set(it, "PATH_INFO", &req.path);
    set(it, "REQUEST_SCHEME", "http");
    set(it, "HTTPS", "");
    set(it, "SERVER_PORT", &cfg.port.to_string());
    set(it, "SERVER_ADDR", &cfg.host);
    set(it, "SERVER_NAME", &cfg.host);
    set(it, "REMOTE_ADDR", &req.remote_addr);
    set(it, "REMOTE_PORT", &req.remote_port.to_string());
    set(it, "REQUEST_TIME", &now.as_secs().to_string());
    set(
        it,
        "REQUEST_TIME_FLOAT",
        &format!("{}.{:06}", now.as_secs(), now.subsec_micros()),
    );
    set(it, "DOCUMENT_ROOT", &cfg.docroot);
    set(it, "SCRIPT_FILENAME", file);
    set(
        it,
        "SCRIPT_NAME",
        &format!(
            "/{}",
            std::path::Path::new(file)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        ),
    );
    set(it, "PHP_SELF", &req.path);
    // php://input — the raw request body, readable as a stream.
    it.php_input = std::rc::Rc::new(req.body.clone());
    for (k, v) in &req.headers {
        let up = k.to_uppercase().replace('-', "_");
        match up.as_str() {
            // CONTENT_TYPE / CONTENT_LENGTH are bare keys in PHP, not HTTP_*.
            "CONTENT_TYPE" | "CONTENT_LENGTH" => set(it, &up, v),
            _ => set(it, &format!("HTTP_{}", up), v),
        }
    }

    let mut get = PhpArray::new();
    parse_query(&req.query, &mut get);
    let mut post = PhpArray::new();
    let ctype_raw = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.as_str())
        .unwrap_or_default();
    let ctype = ctype_raw.to_lowercase();
    let mut files = PhpArray::new();
    if !req.body.is_empty() {
        if ctype.starts_with("application/x-www-form-urlencoded") {
            parse_query(&String::from_utf8_lossy(&req.body), &mut post);
        } else if ctype.starts_with("multipart/form-data") {
            // The boundary is case-sensitive — split on the raw value.
            if let Some(boundary) = ctype_raw.split("boundary=").nth(1) {
                let boundary = boundary.trim().trim_matches('"');
                parse_multipart(&req.body, boundary, &mut post, &mut files, it);
            }
        }
    }
    let mut ck = PhpArray::new();
    if let Some((_, v)) = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("cookie"))
    {
        for pair in v.split(';') {
            let pair = pair.trim();
            if let Some((k, val)) = pair.split_once('=') {
                ck.set(
                    ArrKey::Str(k.trim().into()),
                    Value::bytes(urldecode(val.trim().as_bytes(), false)),
                );
            }
        }
    }
    // $_REQUEST = $_GET ∪ $_POST ∪ $_COOKIE (PHP's default request_order GPC).
    let mut reqarr = get.clone();
    for (k, c) in post.entries.iter().chain(ck.entries.iter()) {
        reqarr.set(k.clone(), c.borrow().clone());
    }
    it.set_superglobal("_GET", get);
    it.set_superglobal("_POST", post);
    it.set_superglobal("_COOKIE", ck);
    it.set_superglobal("_REQUEST", reqarr);
    it.set_superglobal("_FILES", files);
}

/// Parse a multipart/form-data body: field parts fill `post` like
/// urlencoded pairs; `filename=` parts become $_FILES entries with the
/// payload staged in a tmp file (registered on the interp so
/// is_uploaded_file()/move_uploaded_file() recognize it).
fn parse_multipart(
    body: &[u8],
    boundary: &str,
    post: &mut PhpArray,
    files: &mut PhpArray,
    it: &mut Interp,
) {
    let marker = format!("--{}", boundary);
    let text = String::from_utf8_lossy(body);
    for part in text.split(&marker).skip(1) {
        // Parts after the terminal `--` are trailers.
        let part = part.strip_prefix("--").map(|_| "").unwrap_or(part);
        let Some(hdr_end) = part.find("\r\n\r\n") else {
            continue;
        };
        let head = &part[..hdr_end];
        let payload = &part[hdr_end + 4..];
        let payload = payload.strip_suffix("\r\n").unwrap_or(payload);
        let mut name = "";
        let mut filename = "";
        let mut ctype = "";
        for line in head.split("\r\n") {
            if let Some((k, v)) = line.split_once(':') {
                if k.trim().eq_ignore_ascii_case("content-disposition") {
                    for seg in v.split(';') {
                        let seg = seg.trim();
                        if let Some(n) = seg.strip_prefix("name=") {
                            name = n.trim_matches('"');
                        } else if let Some(f) = seg.strip_prefix("filename=") {
                            filename = f.trim_matches('"');
                        }
                    }
                } else if k.trim().eq_ignore_ascii_case("content-type") {
                    ctype = v.trim();
                }
            }
        }
        if name.is_empty() {
            continue;
        }
        if filename.is_empty() {
            // Regular field — same nested-name semantics as urlencoded.
            let v = Value::str(payload.to_string());
            let segs: Vec<String> = if let Some(open) = name.find('[') {
                let mut out = vec![name[..open].to_string()];
                for p in name[open..].split('[') {
                    if let Some(inner) = p.strip_suffix(']') {
                        out.push(inner.to_string());
                    }
                }
                out
            } else {
                vec![name.to_string()]
            };
            if !segs.is_empty() && !segs[0].is_empty() {
                set_nested(post, &segs[0], &segs[1..], v);
            }
        } else {
            // File upload: stage the payload under a PHP-style tmp name.
            let tmp = std::env::temp_dir().join(format!(
                "phpun_upload{:x}{:x}",
                std::process::id(),
                files.entries.len()
            ));
            let err = match std::fs::write(&tmp, payload.as_bytes()) {
                Ok(_) => 0,
                Err(_) => 2,
            };
            if err == 0 {
                it.uploads.push(tmp.clone());
            }
            let mut ent = PhpArray::new();
            let base = std::path::Path::new(filename)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| filename.to_string());
            ent.set(ArrKey::Str("name".into()), Value::str(base.clone()));
            ent.set(
                ArrKey::Str("full_path".into()),
                Value::str(filename.to_string()),
            );
            ent.set(ArrKey::Str("type".into()), Value::str(ctype.to_string()));
            ent.set(
                ArrKey::Str("tmp_name".into()),
                Value::str(tmp.display().to_string()),
            );
            ent.set(ArrKey::Str("error".into()), Value::Int(err));
            ent.set(ArrKey::Str("size".into()), Value::Int(payload.len() as i64));
            let segs: Vec<String> = if let Some(open) = name.find('[') {
                let mut out = vec![name[..open].to_string()];
                for p in name[open..].split('[') {
                    if let Some(inner) = p.strip_suffix(']') {
                        out.push(inner.to_string());
                    }
                }
                out
            } else {
                vec![name.to_string()]
            };
            // PHP's $_FILES shape: $_FILES[field][attr][...nested] — the
            // attribute keys come first, the field-name nesting inside.
            if !segs.is_empty() && !segs[0].is_empty() {
                set_files_entry(files, &segs[0], &segs[1..], ent);
            }
        }
    }
}

/// `a=1&b[x]=2&c[]=3` → PHP-shaped PhpArray. Key chars `.`/space→`_` on
/// the leading segment like PHP does.
fn parse_query(s: &str, arr: &mut PhpArray) {
    for pair in s.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let k = crate::value::lossy(&urldecode(k.as_bytes(), false)).into_owned();
        let v = Value::bytes(urldecode(v.as_bytes(), false));
        // Nested keys: name[seg][seg2]… — first segment is the array name.
        let segs: Vec<String> = if let Some(open) = k.find('[') {
            let mut out = vec![k[..open].to_string()];
            for part in k[open..].split('[') {
                if let Some(inner) = part.strip_suffix(']') {
                    out.push(inner.to_string());
                }
            }
            out
        } else {
            vec![k]
        };
        if segs.is_empty() || segs[0].is_empty() {
            continue;
        }
        let first: String = segs[0]
            .chars()
            .map(|c| if c == '.' || c == ' ' { '_' } else { c })
            .collect();
        set_nested(arr, &first, &segs[1..], v);
    }
}

fn set_nested(arr: &mut PhpArray, first: &str, rest: &[String], v: Value) {
    if rest.is_empty() {
        arr.set(ArrKey::Str(first.into()), v);
        return;
    }
    // Fetch-or-create the nested array under `first`.
    let child = match arr.get_cell(&ArrKey::Str(first.into())) {
        Some(c) => match &*c.borrow() {
            Value::Array(a) => a.clone(),
            _ => {
                let a = std::rc::Rc::new(std::cell::RefCell::new(PhpArray::new()));
                *c.borrow_mut() = Value::Array(a.clone());
                a
            }
        },
        None => {
            let a = std::rc::Rc::new(std::cell::RefCell::new(PhpArray::new()));
            arr.set(ArrKey::Str(first.into()), Value::Array(a.clone()));
            a
        }
    };
    let mut child = child.borrow_mut();
    let seg = &rest[0];
    if rest.len() == 1 {
        if seg.is_empty() {
            child.push(v);
        } else {
            child.set(arr_key_of(seg), v);
        }
    } else {
        set_nested_cell(&mut child, seg, &rest[1..], v);
    }
}

fn set_nested_cell(arr: &mut PhpArray, seg: &str, rest: &[String], v: Value) {
    let k = if seg.is_empty() {
        ArrKey::Int(arr.entries.len() as i64)
    } else {
        arr_key_of(seg)
    };
    if rest.is_empty() {
        arr.set(k, v);
        return;
    }
    let child = match arr.get_cell(&k) {
        Some(c) => match &*c.borrow() {
            Value::Array(a) => a.clone(),
            _ => {
                let a = std::rc::Rc::new(std::cell::RefCell::new(PhpArray::new()));
                *c.borrow_mut() = Value::Array(a.clone());
                a
            }
        },
        None => {
            let a = std::rc::Rc::new(std::cell::RefCell::new(PhpArray::new()));
            arr.set(k.clone(), Value::Array(a.clone()));
            a
        }
    };
    let mut child = child.borrow_mut();
    let seg2 = &rest[0];
    set_nested_cell(&mut child, seg2, &rest[1..], v);
}

/// PHP array key semantics: numeric strings become Int keys.
fn arr_key_of(s: &str) -> ArrKey {
    if let Ok(i) = s.parse::<i64>() {
        ArrKey::Int(i)
    } else {
        ArrKey::Str(s.into())
    }
}

fn reason(code: i64) -> &'static str {
    match code {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        410 => "Gone",
        418 => "I'm a teapot",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

/// $_FILES[field][attr][nested-path]: for each attribute of the upload
/// entry, descend `rest` inside files[field][attr] — f[] uploads group
/// as name[0], name[1], ... like PHP.
fn set_files_entry(files: &mut PhpArray, first: &str, rest: &[String], ent: PhpArray) {
    let slot = match files.get_cell(&ArrKey::Str(first.into())) {
        Some(c) => match &*c.borrow() {
            Value::Array(a) => a.clone(),
            _ => {
                let a = std::rc::Rc::new(std::cell::RefCell::new(PhpArray::new()));
                *c.borrow_mut() = Value::Array(a.clone());
                a
            }
        },
        None => {
            let a = std::rc::Rc::new(std::cell::RefCell::new(PhpArray::new()));
            files.set(ArrKey::Str(first.into()), Value::Array(a.clone()));
            a
        }
    };
    for (attr, v) in ent.entries.iter() {
        let mut slot_b = slot.borrow_mut();
        let attr_arr = match slot_b.get_cell(attr) {
            Some(c) => match &*c.borrow() {
                Value::Array(a) => a.clone(),
                _ => {
                    let a = std::rc::Rc::new(std::cell::RefCell::new(PhpArray::new()));
                    *c.borrow_mut() = Value::Array(a.clone());
                    a
                }
            },
            None => {
                let a = std::rc::Rc::new(std::cell::RefCell::new(PhpArray::new()));
                slot_b.set(attr.clone(), Value::Array(a.clone()));
                a
            }
        };
        let val = v.borrow().clone();
        if rest.is_empty() {
            // Scalar attr slot — PHP flattens single-file fields:
            // $_FILES[f][name] = 'a.txt' not ['name'][0].
            drop(slot_b);
            slot.borrow_mut().set(attr.clone(), val);
        } else {
            let mut segs: Vec<String> = rest.to_vec();
            let seg = segs.remove(0);
            set_nested_cell_files(&mut attr_arr.borrow_mut(), &seg, &segs, val);
        }
    }
}

fn set_nested_cell_files(arr: &mut PhpArray, seg: &str, rest: &[String], v: Value) {
    let k = if seg.is_empty() {
        ArrKey::Int(arr.entries.len() as i64)
    } else {
        arr_key_of(seg)
    };
    if rest.is_empty() {
        arr.set(k, v);
        return;
    }
    let child = match arr.get_cell(&k) {
        Some(c) => match &*c.borrow() {
            Value::Array(a) => a.clone(),
            _ => {
                let a = std::rc::Rc::new(std::cell::RefCell::new(PhpArray::new()));
                *c.borrow_mut() = Value::Array(a.clone());
                a
            }
        },
        None => {
            let a = std::rc::Rc::new(std::cell::RefCell::new(PhpArray::new()));
            arr.set(k.clone(), Value::Array(a.clone()));
            a
        }
    };
    let mut child = child.borrow_mut();
    let seg2 = rest[0].clone();
    set_nested_cell_files(&mut child, &seg2, &rest[1..], v);
}
