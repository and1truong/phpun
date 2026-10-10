//! json_encode/json_decode and their error reporting.

use super::*;
use std::borrow::Cow;

// zend JSON error codes (what json_last_error reports).
const J_DEPTH: i64 = 1;
const J_STATE: i64 = 2;
const J_CTRL: i64 = 3;
const J_SYNTAX: i64 = 4;
const J_UTF8: i64 = 5;
const J_RECURSION: i64 = 6;
const J_INF_NAN: i64 = 7;
const J_UNSUPPORTED: i64 = 8;
const J_PROPNAME: i64 = 9;
const J_UTF16: i64 = 10;

// Encode flag bits. Decode shares zend's overlapping space —
// JSON_OBJECT_AS_ARRAY = 1 and JSON_BIGINT_AS_STRING = 2 are read inline.
const F_HEX_TAG: i64 = 1;
const F_HEX_AMP: i64 = 2;
const F_HEX_APOS: i64 = 4;
const F_HEX_QUOT: i64 = 8;
const F_FORCE_OBJECT: i64 = 16;
const F_NUMERIC: i64 = 32;
const F_UNESC_SLASH: i64 = 64;
const F_PRETTY: i64 = 128;
const F_UNESC_UNI: i64 = 256;
const F_PARTIAL: i64 = 512;
const F_PRESERVE: i64 = 1024;
const F_UNESC_LT: i64 = 2048;
const F_UTF8_IGNORE: i64 = 0x100000;
const F_UTF8_SUB: i64 = 0x200000;
const F_THROW: i64 = 0x400000;

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
    Ok(Some(match name {
        "json_encode" => {
            let v = arg(args, 0);
            let flags = arg(args, 1).to_int();
            let depth = if args.len() > 2 {
                arg(args, 2).to_int()
            } else {
                512
            };
            // zend resets last_json_error at body start UNLESS THROW is
            // effective (THROW without PARTIAL) — under it the global is
            // never touched, not even on success.
            let throw = flags & F_THROW != 0 && flags & F_PARTIAL == 0;
            if !throw {
                it.last_json_error = 0;
            }
            let (s, code) = json_encode(it, &v, flags, depth)?;
            if code != 0 && throw {
                let e = it.exception_code("JsonException", json_err_msg(code), code);
                return Err(it.throw_value(e));
            }
            if !throw {
                it.last_json_error = code;
            }
            if code != 0 && flags & F_PARTIAL == 0 {
                Value::Bool(false)
            } else {
                Value::str(s)
            }
        }
        "json_decode" => {
            // Borrow the input when it's already a string — arg_bs
            // copies the whole document otherwise.
            let s0 = arg(args, 0);
            let owned;
            let s: &[u8] = match &s0 {
                Value::Str(s) => s,
                _ => {
                    owned = arg_bs(it, args, 0);
                    &owned
                }
            };
            // An explicit assoc wins; OBJECT_AS_ARRAY only fills the
            // default when arg 2 is null/missing (zend parity).
            let assoc = if matches!(arg(args, 1), Value::Null) {
                arg(args, 3).to_int() & 1 != 0
            } else {
                arg(args, 1).is_truthy()
            };
            let depth = if args.len() > 2 {
                arg(args, 2).to_int()
            } else {
                512
            };
            let flags = arg(args, 3).to_int();
            // zend treats an empty document as an immediate syntax
            // error, checked before the body start (and before depth).
            if s.is_empty() {
                if flags & F_THROW != 0 {
                    let e = it.exception_code("JsonException", json_err_msg(J_SYNTAX), J_SYNTAX);
                    return Err(it.throw_value(e));
                }
                it.last_json_error = J_SYNTAX;
                return Ok(Some(Value::Null));
            }
            // Post-ZPP body start: reset unless THROW — then even the
            // depth ValueErrors below leave err=0 (not the stale code).
            if flags & F_THROW == 0 {
                it.last_json_error = 0;
            }
            if depth < 1 {
                let e = it.exception(
                    "ValueError",
                    "json_decode(): Argument #3 ($depth) must be greater than 0",
                );
                return Err(it.throw_value(e));
            }
            if depth > i32::MAX as i64 {
                let e = it.exception(
                    "ValueError",
                    "json_decode(): Argument #3 ($depth) must be less than 2147483647",
                );
                return Err(it.throw_value(e));
            }
            match json_decode(it, s, assoc, depth, flags) {
                Ok(v) => v,
                Err(code) => {
                    if flags & F_THROW != 0 {
                        let e = it.exception_code("JsonException", json_err_msg(code), code);
                        return Err(it.throw_value(e));
                    }
                    it.last_json_error = code;
                    Value::Null
                }
            }
        }
        "json_last_error" => Value::Int(it.last_json_error),
        "json_last_error_msg" => Value::str(json_err_msg(it.last_json_error).to_string()),
        "json_validate" => {
            let s0 = arg(args, 0);
            let owned;
            let s: &[u8] = match &s0 {
                Value::Str(s) => s,
                _ => {
                    owned = arg_bs(it, args, 0);
                    &owned
                }
            };
            let depth = if args.len() > 1 {
                arg(args, 1).to_int()
            } else {
                512
            };
            let flags = arg(args, 2).to_int();
            // zend whitelists validate's flags BEFORE the body start — a
            // bad flag leaves the stale code; depth errors after it see 0.
            if flags & !F_UTF8_IGNORE != 0 {
                let e = it.exception(
                    "ValueError",
                    "json_validate(): Argument #3 ($flags) must be a valid flag (allowed flags: JSON_INVALID_UTF8_IGNORE)",
                );
                return Err(it.throw_value(e));
            }
            // Empty document short-circuits to syntax error before depth.
            if s.is_empty() {
                it.last_json_error = J_SYNTAX;
                return Ok(Some(Value::Bool(false)));
            }
            it.last_json_error = 0;
            if depth < 1 {
                let e = it.exception(
                    "ValueError",
                    "json_validate(): Argument #2 ($depth) must be greater than 0",
                );
                return Err(it.throw_value(e));
            }
            if depth > i32::MAX as i64 {
                let e = it.exception(
                    "ValueError",
                    "json_validate(): Argument #2 ($depth) must be less than 2147483647",
                );
                return Err(it.throw_value(e));
            }
            match json_decode(it, s, true, depth, flags) {
                Ok(_) => {
                    it.last_json_error = 0;
                    Value::Bool(true)
                }
                Err(code) => {
                    it.last_json_error = code;
                    Value::Bool(false)
                }
            }
        }
        _ => return Ok(None),
    }))
}

// ----- helpers -----

fn json_err_msg(code: i64) -> &'static str {
    match code {
        0 => "No error",
        1 => "Maximum stack depth exceeded",
        2 => "State mismatch (invalid or malformed JSON)",
        3 => "Control character error, possibly incorrectly encoded",
        4 => "Syntax error",
        5 => "Malformed UTF-8 characters, possibly incorrectly encoded",
        6 => "Recursion detected",
        7 => "Inf and NaN cannot be JSON encoded",
        8 => "Type is not supported",
        9 => "The decoded property name is invalid",
        10 => "Single unpaired UTF-16 surrogate in unicode escape",
        _ => "Unknown error",
    }
}

// Per-call escape table — one flags→action pass instead of re-matching
// flags per char (60-json writes ~200KB of string content per rep).
const A_HI: u8 = 1; // >=0x80: utf8-validate, then maybe \uXXXX
const A_UNI_LO: u8 = 2; // \u00xx lowercase (control chars, non-ascii)
const A_UNI_HI: u8 = 3; // \u00XX uppercase (JSON_HEX_* flags only)
const A_ESC: u8 = 4; // one-char escape: \" \\ \/ \n \r \t \b \f

struct Esc {
    flags: i64,
    tab: [u8; 256],
}

impl Esc {
    fn new(flags: i64) -> Esc {
        let mut tab = [0u8; 256];
        for (c, t) in tab.iter_mut().enumerate() {
            *t = match c as u8 {
                0x00..=0x1f => A_UNI_LO,
                0x80..=0xff => A_HI,
                _ => 0,
            };
        }
        for c in [b'\n', b'\r', b'\t', 0x08, 0x0c] {
            tab[c as usize] = A_ESC;
        }
        tab[b'\\' as usize] = A_ESC;
        tab[b'"' as usize] = if flags & F_HEX_QUOT != 0 {
            A_UNI_HI
        } else {
            A_ESC
        };
        if flags & F_UNESC_SLASH == 0 {
            tab[b'/' as usize] = A_ESC;
        }
        if flags & F_HEX_TAG != 0 {
            tab[b'<' as usize] = A_UNI_HI;
            tab[b'>' as usize] = A_UNI_HI;
        }
        if flags & F_HEX_AMP != 0 {
            tab[b'&' as usize] = A_UNI_HI;
        }
        if flags & F_HEX_APOS != 0 {
            tab[b'\'' as usize] = A_UNI_HI;
        }
        Esc { flags, tab }
    }

    fn pretty(&self) -> bool {
        self.flags & F_PRETTY != 0
    }
}

/// Per-call encode context: flag table, depth ceiling, native-stack
/// budget, and PARTIAL's last-seen error code. The recursion stack
/// itself lives on the Interp (json_enc_stack) so a JsonSerializable
/// body re-entering json_encode sees it — zend marks the container
/// busy for the whole nested call.
struct Enc<'x> {
    esc: &'x Esc,
    max: i64,
    lim: i64,
    err: i64,
}

/// Bytes each container level costs against zend.max_allowed_stack_size
/// (three Rust frames per level; zend charges its own encoder frames).
const LEVEL_BYTES: i64 = 1024;

/// A json error code, or a real exception thrown by jsonSerialize —
/// those propagate straight through the encoder (bug73113/68992), never
/// substituted, not even under PARTIAL.
enum EncErr {
    Json(i64),
    Php(PhpError),
}

impl From<i64> for EncErr {
    fn from(code: i64) -> EncErr {
        EncErr::Json(code)
    }
}

/// Returns (output, zend error code) — under PARTIAL_OUTPUT_ON_ERROR the
/// string still ships and `code` reports the LAST substituted error.
fn json_encode(
    it: &mut Interp,
    v: &Value,
    flags: i64,
    max_depth: i64,
) -> Result<(String, i64), PhpError> {
    let esc = Esc::new(flags);
    // ponytail: fixed 4KB head start — not sized to the payload.
    let mut out = String::with_capacity(4096);
    // ponytail: when zend.max_allowed_stack_size is unset (zend's
    // "use the real stack" mode), cap at 4MB — beyond ~8K levels the
    // native thread stack dies before any useful payload anyway.
    let ini = it.ini_bytes("zend.max_allowed_stack_size");
    let mut cx = Enc {
        esc: &esc,
        max: max_depth,
        lim: if ini > 0 { ini } else { 4 << 20 },
        err: 0,
    };
    match json_enc(it, v, &mut cx, &mut out, 0) {
        Err(EncErr::Php(e)) => return Err(e),
        Err(EncErr::Json(code)) => {
            cx.err = code;
            if flags & F_PARTIAL != 0 {
                out.clear();
                out.push_str(json_subst(code));
            }
        }
        Ok(()) => {}
    }
    Ok((out, cx.err))
}

/// zend's PARTIAL placeholder: `0` for the one error that is a value.
fn json_subst(code: i64) -> &'static str {
    if code == J_INF_NAN {
        "0"
    } else {
        "null"
    }
}

fn json_enc(
    it: &mut Interp,
    v: &Value,
    cx: &mut Enc,
    out: &mut String,
    lvl: i64,
) -> Result<(), EncErr> {
    use std::fmt::Write;
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => write!(out, "{i}").unwrap(),
        Value::Float(f) => {
            if f.is_nan() || f.is_infinite() {
                return Err(J_INF_NAN.into());
            }
            json_f64(*f, cx.esc.flags, out);
        }
        Value::Str(s) => {
            if cx.esc.flags & F_NUMERIC != 0 {
                match numeric(s) {
                    Numeric::Int(i) => {
                        write!(out, "{i}").unwrap();
                        return Ok(());
                    }
                    Numeric::Float(f) if f.is_finite() => {
                        json_f64(f, cx.esc.flags, out);
                        return Ok(());
                    }
                    _ => {}
                }
            }
            json_str(s, cx.esc, out)?
        }
        Value::Array(a) => {
            let lvl = lvl + 1;
            if lvl * LEVEL_BYTES > cx.lim {
                return Err(J_DEPTH.into());
            }
            // zend depth-checks AFTER encoding children: under PARTIAL
            // the cap is ignored and every over-depth frame writes err=1
            // on unwind, so err1 always wins over deeper errors.
            let over = lvl > cx.max;
            if over && cx.esc.flags & F_PARTIAL == 0 {
                return Err(J_DEPTH.into());
            }
            let key = Rc::as_ptr(a) as usize;
            if it.json_enc_stack.contains(&key) {
                return Err(J_RECURSION.into());
            }
            it.json_enc_stack.push(key);
            let r = json_arr(it, a, cx, out, lvl);
            it.json_enc_stack.pop();
            if over {
                cx.err = J_DEPTH;
            }
            return r;
        }
        Value::Object(o) => {
            let lvl = lvl + 1;
            if lvl * LEVEL_BYTES > cx.lim {
                return Err(J_DEPTH.into());
            }
            let over = lvl > cx.max;
            if over && cx.esc.flags & F_PARTIAL == 0 {
                return Err(J_DEPTH.into());
            }
            let key = Rc::as_ptr(o) as usize;
            if it.json_enc_stack.contains(&key) {
                return Err(J_RECURSION.into());
            }
            it.json_enc_stack.push(key);
            let r = if it
                .find_method_in(&o.borrow().class, "jsonserialize")
                .is_some()
            {
                match it.method_invoke(o.clone(), "jsonSerialize", crate::interp::CallArgs::empty())
                {
                    Err(e) => Err(EncErr::Php(e)),
                    Ok(v) => {
                        if matches!(&v, Value::Object(r) if Rc::ptr_eq(r, o)) {
                            // gh10519: serialize() returning $this encodes the
                            // property view, once. Anything else containing $this
                            // hits the stack check above.
                            json_obj(it, o, cx, out, lvl)
                        } else {
                            json_enc(it, &v, cx, out, lvl)
                        }
                    }
                }
            } else {
                json_obj(it, o, cx, out, lvl)
            };
            it.json_enc_stack.pop();
            if over {
                cx.err = J_DEPTH;
            }
            return r;
        }
        Value::Callable(_) => out.push_str("{}"),
        _ => return Err(J_UNSUPPORTED.into()),
    }
    Ok(())
}

/// Element/property encoder: PARTIAL swaps a failing child for 0/null and
/// records its code — zend reports the last error it hit.
fn json_child(
    it: &mut Interp,
    v: &Value,
    cx: &mut Enc,
    out: &mut String,
    lvl: i64,
) -> Result<(), EncErr> {
    let start = out.len();
    match json_enc(it, v, cx, out, lvl) {
        Err(EncErr::Json(code)) if cx.esc.flags & F_PARTIAL != 0 => {
            cx.err = code;
            out.truncate(start);
            out.push_str(json_subst(code));
            Ok(())
        }
        r => r,
    }
}

fn indent(out: &mut String, lvl: i64) {
    out.extend(std::iter::repeat_n(' ', 4 * lvl.max(0) as usize));
}

fn json_arr(
    it: &mut Interp,
    a: &Rc<RefCell<PhpArray>>,
    cx: &mut Enc,
    out: &mut String,
    lvl: i64,
) -> Result<(), EncErr> {
    // Snapshot the entries before any element runs: a JsonSerializable
    // body can unset an element of this very array (bug77843) — holding
    // the borrow across the loop would double-borrow panic. Tombstones
    // are dead buckets, not elements (009 keeps a list after unset).
    let (live, is_list) = {
        let a = a.borrow();
        let live: Vec<(ArrKey, Cell)> = a
            .entries
            .iter()
            .filter(|(k, _)| !matches!(k, ArrKey::Tomb))
            .cloned()
            .collect();
        let is_list = cx.esc.flags & F_FORCE_OBJECT == 0
            && live
                .iter()
                .enumerate()
                .all(|(i, (k, _))| matches!(k, ArrKey::Int(x) if *x == i as i64));
        (live, is_list)
    };
    out.push(if is_list { '[' } else { '{' });
    let mut first = true;
    for (k, c) in live {
        if !first {
            out.push(',');
        }
        first = false;
        if cx.esc.pretty() {
            out.push('\n');
            indent(out, lvl);
        }
        if !is_list {
            json_key(&k, cx.esc, out)?;
            out.push(':');
            if cx.esc.pretty() {
                out.push(' ');
            }
        }
        let v = c.borrow().clone();
        json_child(it, &v, cx, out, lvl)?;
    }
    if cx.esc.pretty() && !first {
        out.push('\n');
        indent(out, lvl - 1);
    }
    out.push(if is_list { ']' } else { '}' });
    Ok(())
}

/// Property view of a plain object (and the JsonSerializable $this
/// fallback): same shape zend emits.
fn json_obj(
    it: &mut Interp,
    o: &Rc<RefCell<PhpObject>>,
    cx: &mut Enc,
    out: &mut String,
    lvl: i64,
) -> Result<(), EncErr> {
    let ao_arr = if matches!(
        o.borrow().internal,
        Some(crate::value::ObjectInternal::ArrayIter { .. })
    ) {
        Some(it.ao_arr(o))
    } else {
        None
    };
    let dtp = if ao_arr.is_none() {
        crate::builtins::datetime::dt_public_props(&o.borrow())
    } else {
        None
    };
    out.push('{');
    let mut first = true;
    if let Some(arr) = ao_arr {
        let entries: Vec<(ArrKey, Cell)> = arr.borrow().iter().cloned().collect();
        for (k, c) in entries {
            if !first {
                out.push(',');
            }
            first = false;
            if cx.esc.pretty() {
                out.push('\n');
                indent(out, lvl);
            }
            json_key(&k, cx.esc, out)?;
            out.push(':');
            if cx.esc.pretty() {
                out.push(' ');
            }
            let v = c.borrow().clone();
            json_child(it, &v, cx, out, lvl)?;
        }
    } else if let Some(dtp) = dtp {
        for (k, v) in dtp.iter() {
            if !first {
                out.push(',');
            }
            first = false;
            if cx.esc.pretty() {
                out.push('\n');
                indent(out, lvl);
            }
            json_str(k.as_bytes(), cx.esc, out)?;
            out.push(':');
            if cx.esc.pretty() {
                out.push(' ');
            }
            json_child(it, v, cx, out, lvl)?;
        }
    } else {
        let entries = it.object_serial_entries(o);
        for (name, slot, decl) in entries {
            if !decl
                .as_ref()
                .map(|(p, _)| p.visibility == crate::ast::Visibility::Public)
                .unwrap_or(true)
            {
                continue;
            }
            let name = match crate::value::int_prop_index(&name) {
                Some(i) => i.to_string(),
                None => name,
            };
            let v = match &decl {
                Some((p, dcls)) => {
                    let v = it.serial_entry_value(o, p, dcls, &slot);
                    // serial_entry_value swallows a get-hook throw — the
                    // throwable survives in pending_exception; zend
                    // releases it out of the encoder (hooked prop test).
                    match it.take_pending_exception() {
                        Some(x) => return Err(EncErr::Php(it.throw_value(x))),
                        None => v,
                    }
                }
                None => o.borrow().props.get(&slot).map(|c| c.borrow().clone()),
            };
            if let Some(v) = v {
                if !first {
                    out.push(',');
                }
                first = false;
                if cx.esc.pretty() {
                    out.push('\n');
                    indent(out, lvl);
                }
                json_str(name.as_bytes(), cx.esc, out)?;
                out.push(':');
                if cx.esc.pretty() {
                    out.push(' ');
                }
                json_child(it, &v, cx, out, lvl)?;
            }
        }
    }
    if cx.esc.pretty() && !first {
        out.push('\n');
        indent(out, lvl - 1);
    }
    out.push('}');
    Ok(())
}

fn json_key(key: &ArrKey, esc: &Esc, out: &mut String) -> Result<(), i64> {
    match key {
        ArrKey::Str(s) => json_str(s.as_bytes(), esc, out),
        ArrKey::Int(i) => {
            use std::fmt::Write;
            write!(out, "\"{i}\"").unwrap();
            Ok(())
        }
        ArrKey::Tomb => Ok(()),
    }
}

/// Emit `"…"`. Err(5) on broken utf8 unless an INVALID_UTF8_* flag says
/// what to substitute instead.
fn json_str(s: &[u8], esc: &Esc, out: &mut String) -> Result<(), i64> {
    use std::fmt::Write;
    out.push('"');
    let mut i = 0;
    let mut run = 0;
    while i < s.len() {
        let c = s[i];
        let act = esc.tab[c as usize];
        if act == 0 {
            i += 1;
            continue;
        }
        if run < i {
            // clean runs are all-ASCII — hi bytes break them above
            out.push_str(std::str::from_utf8(&s[run..i]).unwrap());
        }
        match act {
            A_ESC => {
                out.push('\\');
                out.push(match c {
                    b'\n' => 'n',
                    b'\r' => 'r',
                    b'\t' => 't',
                    0x08 => 'b',
                    0x0c => 'f',
                    _ => c as char, // \" \\ \/
                });
                i += 1;
            }
            A_UNI_LO => {
                write!(out, "\\u{:04x}", c).unwrap();
                i += 1;
            }
            A_UNI_HI => {
                write!(out, "\\u{:04X}", c).unwrap();
                i += 1;
            }
            _ => {
                let n = utf8_len(c);
                if i + n <= s.len() && std::str::from_utf8(&s[i..i + n]).is_ok() {
                    let u = std::str::from_utf8(&s[i..i + n])
                        .unwrap()
                        .chars()
                        .next()
                        .unwrap() as u32;
                    if esc.flags & F_UNESC_UNI != 0 {
                        // U+2028/29 stay escaped — JS line separators.
                        if (u == 0x2028 || u == 0x2029) && esc.flags & F_UNESC_LT == 0 {
                            write!(out, "\\u{:04x}", u).unwrap();
                        } else {
                            out.push_str(std::str::from_utf8(&s[i..i + n]).unwrap());
                        }
                    } else if u > 0xffff {
                        let x = u - 0x10000;
                        write!(
                            out,
                            "\\u{:04x}\\u{:04x}",
                            0xd800 + (x >> 10),
                            0xdc00 + (x & 0x3ff)
                        )
                        .unwrap();
                    } else {
                        write!(out, "\\u{:04x}", u).unwrap();
                    }
                    i += n;
                } else if esc.flags & F_UTF8_IGNORE != 0 {
                    i += bad_seq_len(s, i);
                } else if esc.flags & F_UTF8_SUB != 0 {
                    // The substitute is a real U+FFFD — it goes out raw
                    // under UNESCAPED_UNICODE like any other char.
                    if esc.flags & F_UNESC_UNI != 0 {
                        out.push('\u{fffd}');
                    } else {
                        out.push_str("\\ufffd");
                    }
                    i += bad_seq_len(s, i);
                } else {
                    return Err(J_UTF8);
                }
            }
        }
        run = i;
    }
    if run < s.len() {
        out.push_str(std::str::from_utf8(&s[run..]).unwrap());
    }
    out.push('"');
    Ok(())
}

fn json_f64(f: f64, flags: i64, out: &mut String) {
    let mut s = format_float_repr(f);
    if s.contains('E') {
        // gcvt prints %G's uppercase; zend's JSON writer uses 'e'.
        s = s.replace('E', "e");
    }
    if flags & F_PRESERVE != 0 && !s.contains('.') && !s.contains('e') {
        s.push_str(".0");
    }
    out.push_str(&s);
}

// ----- decode -----

fn json_decode(
    it: &mut Interp,
    s: &[u8],
    assoc: bool,
    depth: i64,
    flags: i64,
) -> Result<Value, i64> {
    let cx = Dec { assoc, flags };
    let mut pos = 0;
    let v = json_value(it, s, &mut pos, &cx, depth, 0)?;
    json_ws(s, &mut pos)?;
    match s.get(pos) {
        // ws already rejected <0x20 and broken utf8 in the tail.
        None => Ok(v),
        Some(_) => Err(J_SYNTAX),
    }
}

fn json_ws(b: &[u8], pos: &mut usize) -> Result<(), i64> {
    while let Some(&c) = b.get(*pos) {
        match c {
            b' ' | b'\t' | b'\r' | b'\n' => *pos += 1,
            _ if c < 0x20 => return Err(J_CTRL),
            _ if c >= 0x80 => {
                // zend's scanner validates utf8 wherever it lands — a
                // broken sequence anywhere mid-document is err5, a valid
                // char simply isn't whitespace.
                if utf8_seq_len(b, *pos).is_none() {
                    return Err(J_UTF8);
                }
                return Ok(());
            }
            _ => return Ok(()),
        }
    }
    Ok(())
}

/// Per-call decode context — assoc/flags are fixed for the document.
struct Dec {
    assoc: bool,
    flags: i64,
}

/// zend's decoder stack budget, reverse-engineered from the oracle:
/// [`[` costs 2499 units, `{` 4998; a pure `[[[` chain trips at nest
/// 4999 (err4), `{"a":{"a":` at 2500, strict `[{"a":` alternation at
/// 1667. ponytail: the real bound is zend's C-stack measurement whose
/// per-frame cost varies by parser path — exotic mixes can land a few
/// percent off; uniform and alternating patterns are exact.
const DEC_STACK: i64 = 4998 * 2499;
const DEC_ARR_COST: i64 = 2499;
const DEC_OBJ_COST: i64 = 4998;

fn json_value(
    it: &mut Interp,
    b: &[u8],
    pos: &mut usize,
    cx: &Dec,
    rem: i64,
    stk: i64,
) -> Result<Value, i64> {
    json_ws(b, pos)?;
    match b.get(*pos) {
        Some(b'n') => json_lit(b, pos, b"null", Value::Null),
        Some(b't') => json_lit(b, pos, b"true", Value::Bool(true)),
        Some(b'f') => json_lit(b, pos, b"false", Value::Bool(false)),
        Some(b'"') => {
            *pos += 1;
            Ok(Value::bytes(jstr(b, pos, cx.flags)?.into_owned()))
        }
        Some(b'[') => {
            *pos += 1;
            // zend counts one extra level past the deepest container.
            let rem = rem - 1;
            let stk = stk + DEC_ARR_COST;
            if rem == 0 {
                return Err(J_DEPTH);
            }
            if stk > DEC_STACK {
                return Err(J_SYNTAX);
            }
            let mut a = PhpArray::new();
            json_ws(b, pos)?;
            if b.get(*pos) == Some(&b']') {
                *pos += 1;
                return Ok(Value::Array(Rc::new(RefCell::new(a))));
            }
            if b.get(*pos) == Some(&b'}') {
                // A closer of the other kind where one is legal is a
                // state mismatch (err2), not a syntax error.
                return Err(J_STATE);
            }
            loop {
                let v = json_value(it, b, pos, cx, rem, stk)?;
                a.push(v);
                json_ws(b, pos)?;
                match b.get(*pos) {
                    Some(b',') => *pos += 1,
                    Some(b']') => {
                        *pos += 1;
                        break;
                    }
                    Some(b'}') => return Err(J_STATE),
                    _ => return Err(J_SYNTAX),
                }
            }
            Ok(Value::Array(Rc::new(RefCell::new(a))))
        }
        Some(b'{') => {
            *pos += 1;
            let rem = rem - 1;
            let stk = stk + DEC_OBJ_COST;
            if rem == 0 {
                return Err(J_DEPTH);
            }
            if stk > DEC_STACK {
                return Err(J_SYNTAX);
            }
            json_ws(b, pos)?;
            if b.get(*pos) == Some(&b'}') {
                *pos += 1;
                return Ok(if cx.assoc {
                    Value::Array(Rc::new(RefCell::new(PhpArray::new())))
                } else {
                    stdclass(it, HashMap::new(), Vec::new())
                });
            }
            if b.get(*pos) == Some(&b']') {
                return Err(J_STATE);
            }
            if cx.assoc {
                let mut a = PhpArray::new();
                loop {
                    json_ws(b, pos)?;
                    if b.get(*pos) != Some(&b'"') {
                        return Err(J_SYNTAX);
                    }
                    *pos += 1;
                    let k = key_cast(&jstr(b, pos, cx.flags)?)?;
                    json_ws(b, pos)?;
                    if b.get(*pos) != Some(&b':') {
                        return Err(J_SYNTAX);
                    }
                    *pos += 1;
                    let v = json_value(it, b, pos, cx, rem, stk)?;
                    a.set(k, v);
                    json_ws(b, pos)?;
                    match b.get(*pos) {
                        Some(b',') => *pos += 1,
                        Some(b'}') => {
                            *pos += 1;
                            break;
                        }
                        Some(b']') => return Err(J_STATE),
                        _ => return Err(J_SYNTAX),
                    }
                }
                Ok(Value::Array(Rc::new(RefCell::new(a))))
            } else {
                // stdClass: build props directly, no temp array pass.
                let mut props = HashMap::new();
                let mut order = Vec::new();
                loop {
                    json_ws(b, pos)?;
                    if b.get(*pos) != Some(&b'"') {
                        return Err(J_SYNTAX);
                    }
                    *pos += 1;
                    let k = String::from_utf8(jstr(b, pos, cx.flags)?.into_owned())
                        .map_err(|_| J_UTF8)?;
                    json_ws(b, pos)?;
                    if b.get(*pos) != Some(&b':') {
                        return Err(J_SYNTAX);
                    }
                    *pos += 1;
                    let v = json_value(it, b, pos, cx, rem, stk)?;
                    // \0-prefixed names are zend's private-prop mangling
                    // form — illegal as a decoded member (bug68546).
                    if k.starts_with('\0') {
                        return Err(J_PROPNAME);
                    }
                    if !props.contains_key(&k) {
                        order.push(k.clone());
                    }
                    props.insert(k, Rc::new(RefCell::new(v)));
                    json_ws(b, pos)?;
                    match b.get(*pos) {
                        Some(b',') => *pos += 1,
                        Some(b'}') => {
                            *pos += 1;
                            break;
                        }
                        Some(b']') => return Err(J_STATE),
                        _ => return Err(J_SYNTAX),
                    }
                }
                Ok(stdclass(it, props, order))
            }
        }
        Some(&c) if c == b'-' || c.is_ascii_digit() => jnum(b, pos, cx.flags),
        _ => Err(J_SYNTAX),
    }
}

fn stdclass(it: &mut Interp, props: HashMap<String, Cell>, order: Vec<String>) -> Value {
    let Some(cls) = it.lookup_class("stdclass") else {
        return Value::Null;
    };
    Value::Object(it.alloc_obj(PhpObject {
        class: cls,
        props,
        prop_order: order,
        id: 0,
        internal: None,
        unset_props: std::collections::HashSet::new(),
    }))
}

fn json_lit(b: &[u8], pos: &mut usize, lit: &[u8], v: Value) -> Result<Value, i64> {
    if b[*pos..].starts_with(lit) {
        *pos += lit.len();
        Ok(v)
    } else {
        Err(J_SYNTAX)
    }
}

/// zend's object-key int cast for assoc decode: canonical decimal forms
/// only — "+3", "007", "-0", "1e5" and int64 overflows stay strings.
fn key_cast(k: &[u8]) -> Result<ArrKey, i64> {
    let s = std::str::from_utf8(k).map_err(|_| J_UTF8)?;
    let d = s.strip_prefix('-').unwrap_or(s);
    if !d.is_empty()
        && d.bytes().all(|c| c.is_ascii_digit())
        && !(d.len() > 1 && d.starts_with('0'))
        && !(s.starts_with('-') && d == "0")
    {
        if let Ok(i) = s.parse::<i64>() {
            return Ok(ArrKey::Int(i));
        }
    }
    Ok(ArrKey::Str(s.into()))
}

/// -?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)? — zend rejects "01",
/// ".5", "1.", "1e" as syntax (err4); int64 overflow becomes float, or
/// string under JSON_BIGINT_AS_STRING.
fn jnum(b: &[u8], pos: &mut usize, flags: i64) -> Result<Value, i64> {
    let start = *pos;
    let mut p = start;
    if b.get(p) == Some(&b'-') {
        p += 1;
    }
    match b.get(p) {
        Some(b'0') => p += 1,
        Some(&c) if c.is_ascii_digit() => {
            p += 1;
            while matches!(b.get(p), Some(&c) if c.is_ascii_digit()) {
                p += 1;
            }
        }
        _ => return Err(J_SYNTAX),
    }
    let mut float = false;
    if b.get(p) == Some(&b'.') {
        float = true;
        p += 1;
        if !matches!(b.get(p), Some(&c) if c.is_ascii_digit()) {
            return Err(J_SYNTAX);
        }
        while matches!(b.get(p), Some(&c) if c.is_ascii_digit()) {
            p += 1;
        }
    }
    if matches!(b.get(p), Some(b'e') | Some(b'E')) {
        float = true;
        p += 1;
        if matches!(b.get(p), Some(b'+') | Some(b'-')) {
            p += 1;
        }
        if !matches!(b.get(p), Some(&c) if c.is_ascii_digit()) {
            return Err(J_SYNTAX);
        }
        while matches!(b.get(p), Some(&c) if c.is_ascii_digit()) {
            p += 1;
        }
    }
    let text = std::str::from_utf8(&b[start..p]).unwrap();
    *pos = p;
    if !float {
        if let Ok(i) = text.parse::<i64>() {
            return Ok(Value::Int(i));
        }
        if flags & 2 != 0 {
            // JSON_BIGINT_AS_STRING
            return Ok(Value::bytes(b[start..p].to_vec()));
        }
    }
    Ok(Value::Float(text.parse().unwrap_or(f64::INFINITY)))
}

/// String-content scanner — caller consumed the opening `"`; on return
/// `*pos` sits past the closing quote. Fast path borrows the span when no
/// escapes; unterminated input lands in zend's CTRL bucket (err3).
fn jstr<'a>(b: &'a [u8], pos: &mut usize, flags: i64) -> Result<Cow<'a, [u8]>, i64> {
    let mut p = *pos;
    let mut hi = false;
    loop {
        match b.get(p) {
            None => return Err(J_CTRL),
            Some(b'"') => break,
            Some(b'\\') => return jstr_esc(b, pos, p, flags),
            Some(&c) if c < 0x20 => return Err(J_CTRL),
            Some(&c) => {
                hi |= c >= 0x80;
                p += 1;
            }
        }
    }
    let span = &b[*pos..p];
    *pos = p + 1;
    if !hi {
        return Ok(Cow::Borrowed(span));
    }
    match std::str::from_utf8(span) {
        Ok(_) => Ok(Cow::Borrowed(span)),
        Err(_) if flags & (F_UTF8_IGNORE | F_UTF8_SUB) == 0 => Err(J_UTF8),
        Err(_) => {
            let mut out = Vec::with_capacity(span.len());
            utf8_filter(&mut out, span, flags);
            Ok(Cow::Owned(out))
        }
    }
}

/// Slow path once the first `\` is seen; `p` points at it.
fn jstr_esc<'a>(
    b: &'a [u8],
    pos: &mut usize,
    mut p: usize,
    flags: i64,
) -> Result<Cow<'a, [u8]>, i64> {
    let mut out = Vec::with_capacity(32);
    let mut run = *pos;
    loop {
        match b.get(p) {
            None => return Err(J_CTRL),
            Some(b'"') => {
                push_span(&mut out, &b[run..p], flags)?;
                *pos = p + 1;
                return Ok(Cow::Owned(out));
            }
            Some(b'\\') => {
                push_span(&mut out, &b[run..p], flags)?;
                p += 1;
                match b.get(p) {
                    None => return Err(J_SYNTAX),
                    Some(b'"') => out.push(b'"'),
                    Some(b'\\') => out.push(b'\\'),
                    Some(b'/') => out.push(b'/'),
                    Some(b'n') => out.push(b'\n'),
                    Some(b't') => out.push(b'\t'),
                    Some(b'r') => out.push(b'\r'),
                    Some(b'b') => out.push(0x08),
                    Some(b'f') => out.push(0x0c),
                    Some(b'u') => {
                        let cp = hex4(b, p + 1).ok_or(J_SYNTAX)?;
                        p += 5;
                        let u = if (0xd800..0xdc00).contains(&cp) {
                            // lone high surrogate → UTF16 error; a valid
                            // pair wants \uDC00..\uDFFF right behind.
                            if b.get(p) == Some(&b'\\') && b.get(p + 1) == Some(&b'u') {
                                let lo = hex4(b, p + 2).ok_or(J_SYNTAX)?;
                                if (0xdc00..0xe000).contains(&lo) {
                                    p += 6;
                                    0x10000 + ((cp - 0xd800) << 10) + (lo - 0xdc00)
                                } else {
                                    return Err(J_UTF16);
                                }
                            } else {
                                return Err(J_UTF16);
                            }
                        } else if (0xdc00..0xe000).contains(&cp) {
                            return Err(J_UTF16);
                        } else {
                            cp
                        };
                        let mut tmp = [0u8; 4];
                        out.extend_from_slice(
                            char::from_u32(u)
                                .unwrap_or('\u{fffd}')
                                .encode_utf8(&mut tmp)
                                .as_bytes(),
                        );
                        run = p;
                        continue;
                    }
                    Some(_) => return Err(J_SYNTAX),
                }
                p += 1;
                run = p;
            }
            Some(&c) if c < 0x20 => return Err(J_CTRL),
            Some(_) => p += 1,
        }
    }
}

/// Flush a content run (between escapes) under the utf8 policy.
fn push_span(out: &mut Vec<u8>, span: &[u8], flags: i64) -> Result<(), i64> {
    match std::str::from_utf8(span) {
        Ok(_) => {
            out.extend_from_slice(span);
            Ok(())
        }
        Err(_) if flags & (F_UTF8_IGNORE | F_UTF8_SUB) == 0 => Err(J_UTF8),
        Err(_) => {
            utf8_filter(out, span, flags);
            Ok(())
        }
    }
}

/// Append `s` under INVALID_UTF8_IGNORE/SUBSTITUTE — one U+FFFD per bad
/// BYTE (zend walks one byte forward on every invalid unit).
fn utf8_filter(out: &mut Vec<u8>, s: &[u8], flags: i64) {
    let sub = flags & F_UTF8_SUB != 0 && flags & F_UTF8_IGNORE == 0;
    let mut i = 0;
    while i < s.len() {
        match utf8_seq_len(s, i) {
            Some(n) => {
                out.extend_from_slice(&s[i..i + n]);
                i += n;
            }
            None => {
                if sub {
                    out.extend_from_slice("\u{fffd}".as_bytes());
                }
                i += 1;
            }
        }
    }
}

fn hex4(b: &[u8], i: usize) -> Option<u32> {
    let h = b.get(i..i + 4)?;
    if h.iter().all(|c| c.is_ascii_hexdigit()) {
        u32::from_str_radix(std::str::from_utf8(h).unwrap(), 16).ok()
    } else {
        None
    }
}

/// Length of the valid utf8 char starting at b[i], else None.
fn utf8_seq_len(b: &[u8], i: usize) -> Option<usize> {
    let n = utf8_len(b[i]);
    (i + n <= b.len() && std::str::from_utf8(&b[i..i + n]).is_ok()).then_some(n)
}

/// Size of one broken utf8 "sequence" at s[i] — zend's encoder charges
/// a valid lead byte (C2-F4) plus every continuation byte after it as a
/// single error (one FFFD / one ignored unit); anything else is 1 byte.
/// (Decode and ws-scan still work per byte.)
fn bad_seq_len(s: &[u8], i: usize) -> usize {
    if !(0xc2..=0xf4).contains(&s[i]) {
        return 1;
    }
    let n = utf8_len(s[i]);
    let mut j = i + 1;
    while j < s.len() && j - i < n && s[j] & 0xc0 == 0x80 {
        j += 1;
    }
    j - i
}

fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}
