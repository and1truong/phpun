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

/// `phpun serve <file> --host H --port N`. Blocks until the process dies.
pub fn serve(file: &str, host: &str, port: u16) -> i32 {
    let canon = std::fs::canonicalize(file)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| file.to_string());
    let listener = match TcpListener::bind((host, port)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("phpun serve: cannot bind {host}:{port}: {e}");
            return 1;
        }
    };
    eprintln!("phpun serve: http://{host}:{port} → {canon} (ctrl+c to stop)");
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let f = canon.clone();
                let h = host.to_string();
                std::thread::spawn(move || handle(stream, &f, &h, port));
            }
            Err(e) => eprintln!("phpun serve: accept: {e}"),
        }
    }
    0
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

fn handle(mut stream: TcpStream, file: &str, host: &str, port: u16) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let remote = stream
        .peer_addr()
        .map(|a| (a.ip().to_string(), a.port()))
        .unwrap_or_else(|_| (String::new(), 0));
    match read_request(&mut stream, remote) {
        Ok(Some(req)) => respond(stream, file, host, port, &req),
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
        path: urldecode(&path, true),
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

fn respond(mut stream: TcpStream, file: &str, host: &str, port: u16, req: &Req) {
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
    populate(&mut it, req, file, host, port);
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
    let body = if head { "" } else { it.out.as_str() };
    let mut resp = format!("HTTP/1.1 {} {}\r\n", code, reason(code));
    for h in &lines {
        resp.push_str(h);
        resp.push_str("\r\n");
    }
    resp.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        if head { it.out.len() } else { body.len() },
        body
    ));
    let _ = stream.write_all(resp.as_bytes());
}

/// Populate the request superglobals a PHP web script expects.
fn populate(it: &mut Interp, req: &Req, file: &str, host: &str, port: u16) {
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
    set(it, "SERVER_PORT", &port.to_string());
    set(it, "SERVER_ADDR", host);
    set(it, "SERVER_NAME", host);
    set(it, "REMOTE_ADDR", &req.remote_addr);
    set(it, "REMOTE_PORT", &req.remote_port.to_string());
    set(it, "REQUEST_TIME", &now.as_secs().to_string());
    set(
        it,
        "REQUEST_TIME_FLOAT",
        &format!("{}.{:06}", now.as_secs(), now.subsec_micros()),
    );
    if let Some(docroot) = std::path::Path::new(file).parent() {
        set(it, "DOCUMENT_ROOT", &docroot.display().to_string());
    }
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
    let ctype = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .map(|(_, v)| v.to_lowercase())
        .unwrap_or_default();
    if !req.body.is_empty() && ctype.starts_with("application/x-www-form-urlencoded") {
        parse_query(&String::from_utf8_lossy(&req.body), &mut post);
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
                    Value::str(urldecode(val.trim(), false)),
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
}

/// `a=1&b[x]=2&c[]=3` → PHP-shaped PhpArray. Key chars `.`/space→`_` on
/// the leading segment like PHP does.
fn parse_query(s: &str, arr: &mut PhpArray) {
    for pair in s.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let k = urldecode(k, false);
        let v = Value::str(urldecode(v, false));
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
