//! Formatted output (printf family) and output-buffering (ob_*) builtins.

use super::fs::{stream_open_check, write_ebadf_notice, write_resource, StreamWrite};
use super::*;

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
    Ok(Some(match name {
        "printf" => {
            let s = sprintf_bytes(it, args, 0, "printf", 1)?;
            it.emit_bytes(&s);
            Value::Int(s.len() as i64)
        }
        "sprintf" => {
            let s = sprintf_bytes(it, args, 0, "sprintf", 1)?;
            Value::bytes(s)
        }
        "vsprintf" | "vprintf" => {
            // ZEND_PARSE_PARAMETERS(2, 2)
            if args.len() != 2 {
                return err(
                    "ArgumentCountError",
                    format!(
                        "{}() expects exactly 2 arguments, {} given",
                        name,
                        args.len()
                    ),
                );
            }
            let fmt = fmt_string(it, args, 0, name, 1)?;
            let list = match arg(args, 1) {
                Value::Array(a) => a
                    .borrow()
                    .entries
                    .iter()
                    .map(|(_, c)| c.clone())
                    .collect::<Vec<_>>(),
                v => {
                    return err(
                        "TypeError",
                        format!(
                            "{}(): Argument #2 ($values) must be of type array, {} given",
                            name,
                            zval_word(&v)
                        ),
                    )
                }
            };
            let s = php_formatted_print(it, &fmt, &list, -1, name)?;
            if name == "vprintf" {
                it.emit_bytes(&s);
                Value::Int(s.len() as i64)
            } else {
                Value::bytes(s)
            }
        }
        "fprintf" => {
            // ZEND_PARSE_PARAMETERS(2, -1)
            if args.len() < 2 {
                return err(
                    "ArgumentCountError",
                    format!(
                        "fprintf() expects at least 2 arguments, {} given",
                        args.len()
                    ),
                );
            }
            stream_open_check(args, 0, name, 1, "stream")?;
            let fmt = fmt_string(it, args, 1, "fprintf", 2)?;
            let s = php_formatted_print(it, &fmt, &args[2.min(args.len())..], 2, "fprintf")?;
            // Zend ignores php_stream_write's result — a failed write
            // notices (plain wrapper) or discards (mem ro/input) and
            // fprintf still returns the formatted length.
            if let StreamWrite::Ebadf(errno, msg) = write_resource(it, args.first(), &s)? {
                write_ebadf_notice(it, name, s.len(), errno, &msg)?;
            }
            Value::Int(s.len() as i64)
        }
        "vfprintf" => {
            // ZEND_PARSE_PARAMETERS(3, 3)
            if args.len() != 3 {
                return err(
                    "ArgumentCountError",
                    format!(
                        "vfprintf() expects exactly 3 arguments, {} given",
                        args.len()
                    ),
                );
            }
            stream_open_check(args, 0, name, 1, "stream")?;
            let fmt = fmt_string(it, args, 1, "vfprintf", 2)?;
            let list = match arg(args, 2) {
                Value::Array(a) => a
                    .borrow()
                    .entries
                    .iter()
                    .map(|(_, c)| c.clone())
                    .collect::<Vec<_>>(),
                v => {
                    return err(
                        "TypeError",
                        format!(
                            "vfprintf(): Argument #3 ($values) must be of type array, {} given",
                            zval_word(&v)
                        ),
                    )
                }
            };
            let s = php_formatted_print(it, &fmt, &list, -1, name)?;
            if let StreamWrite::Ebadf(errno, msg) = write_resource(it, args.first(), &s)? {
                write_ebadf_notice(it, name, s.len(), errno, &msg)?;
            }
            Value::Int(s.len() as i64)
        }
        "sprintf_js" | "vsprintf_js" => Value::Null,

        // ----- output buffering -----
        "ob_start" => {
            let h = args.first().map(|c| c.borrow().clone());
            // Zend validates the callback before creating the buffer:
            // string|array|null only, full callable ladder — failure
            // warns, emits the "Failed to create buffer" notice, and
            // returns false without pushing. A throwing autoloader's
            // exception propagates after the diagnostics.
            match &h {
                None | Some(Value::Null) => {}
                Some(v) if it.is_callable_value(v) => {
                    it.take_callable_probe_err();
                }
                Some(v) => {
                    let detail = it.zpp_callback_detail(v);
                    it.warn_pub(&format!("ob_start(): {}", detail))?;
                    it.notice_pub("ob_start(): Failed to create buffer")?;
                    if let Some(pe) = it.take_callable_probe_err() {
                        return Err(pe);
                    }
                    return Ok(Some(Value::Bool(false)));
                }
            }
            it.ob_push(h);
            Value::Bool(true)
        }
        "ob_end_clean" => {
            it.ob_end_clean()?;
            Value::Bool(true)
        }
        "ob_end_flush" => {
            it.ob_end_flush()?;
            Value::Bool(true)
        }
        "ob_get_clean" => it.ob_get_clean(),
        "ob_get_flush" => it.ob_get_flush()?,
        "ob_get_contents" => match it.ob_top() {
            Some(b) => Value::bytes(b.clone()),
            None => Value::Bool(false),
        },
        "ob_get_length" => match it.ob_top() {
            Some(b) => Value::Int(b.len() as i64),
            None => Value::Bool(false),
        },
        "ob_get_level" => Value::Int(it.ob_len() as i64),
        "ob_clean" => {
            it.ob_clean()?;
            Value::Bool(true)
        }
        "ob_flush" | "flush" => {
            it.ob_flush()?;
            Value::Null
        }
        "ob_implicit_flush" | "ob_list_handlers" => Value::Null,
        "ob_get_status" => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
        "output_reset_rewrite_vars" => Value::Bool(true),
        "print" => {
            let s = it.to_string_of(&arg(args, 0));
            it.emit(&s);
            Value::Int(1)
        }
        _ => return Ok(None),
    }))
}

// ----- helpers -----

/// zend's `Z_PARAM_STRING` on the format argument: scalars coerce,
/// null warns (deprecated), objects need __toString, the rest is a
/// TypeError.
fn fmt_string(
    it: &mut Interp,
    args: &[Cell],
    i: usize,
    fname: &str,
    pnum: usize,
) -> Result<Vec<u8>, PhpError> {
    let v = arg(args, i);
    match v {
        Value::Str(s) => Ok(s.to_vec()),
        Value::Int(_) | Value::Float(_) | Value::Bool(_) => it.try_conv_bytes(&v),
        Value::Null => {
            it.deprecated_pub(&format!(
                "{}(): Passing null to parameter #{} ($format) of type string is deprecated",
                fname, pnum
            ))?;
            Ok(Vec::new())
        }
        Value::Object(ref o) => {
            let has_tostring = it.find_method_in(&o.borrow().class, "__tostring").is_some();
            if has_tostring {
                match it.try_conv_bytes(&v) {
                    Ok(b) => Ok(b),
                    Err(e) => {
                        if e.kind == crate::error::ErrorKind::Throw {
                            if let Some(x) = it.take_pending_exception() {
                                return Err(it.throw_value(x));
                            }
                        }
                        Err(e)
                    }
                }
            } else {
                err(
                    "TypeError",
                    format!(
                        "{}(): Argument #{} ($format) must be of type string, {} given",
                        fname,
                        pnum,
                        zval_word(&v)
                    ),
                )
            }
        }
        _ => err(
            "TypeError",
            format!(
                "{}(): Argument #{} ($format) must be of type string, {} given",
                fname,
                pnum,
                zval_word(&v)
            ),
        ),
    }
}

/// sprintf/printf entry: format + variadic args.
fn sprintf_bytes(
    it: &mut Interp,
    args: &[Cell],
    fi: usize,
    fname: &str,
    need: usize,
) -> Result<Vec<u8>, PhpError> {
    if args.len() <= fi {
        return err(
            "ArgumentCountError",
            format!(
                "{}() expects at least {} argument{}, {} given",
                fname,
                need,
                if need == 1 { "" } else { "s" },
                args.len()
            ),
        );
    }
    let fmt = fmt_string(it, args, fi, fname, fi + 1)?;
    php_formatted_print(
        it,
        &fmt,
        &args[(fi + 1).min(args.len())..],
        (fi + 1) as i64,
        fname,
    )
}

/// Port of zend's `php_formatted_print` (ext/standard/formatted_print.c):
/// one byte-faithful parser shared by sprintf/printf/vsprintf/vprintf/
/// fprintf/vfprintf. `nb` is the count of fixed parameters before the
/// format args (1 for sprintf/printf, 2 for fprintf) used in the
/// ArgumentCountError text; -1 switches to the vsprintf-style ValueError.
fn php_formatted_print(
    it: &mut Interp,
    fmt: &[u8],
    args: &[Cell],
    nb: i64,
    fname: &str,
) -> Result<Vec<u8>, PhpError> {
    let mut out: Vec<u8> = Vec::with_capacity(fmt.len() + 16);
    let argc = args.len() as i64;
    let mut currarg: i64 = 0;
    let mut max_missing: i64 = -1;
    let mut pos = 0usize;
    while pos < fmt.len() {
        match fmt[pos..].iter().position(|&c| c == b'%') {
            None => {
                out.extend_from_slice(&fmt[pos..]);
                break;
            }
            Some(off) => {
                out.extend_from_slice(&fmt[pos..pos + off]);
                pos += off;
            }
        }
        pos += 1; // skip '%'
        if pos < fmt.len() && fmt[pos] == b'%' {
            out.push(b'%');
            pos += 1;
            continue;
        }
        // A spec directly after '%' (alpha char) skips all modifiers.
        let mut left = false;
        let mut pad = b' ';
        let mut always_sign = false;
        let mut expprec = false;
        let mut width: i64 = 0;
        let mut precision: i64 = 0;
        let mut adj_prec = false;
        let mut argnum: i64;
        if pos < fmt.len() && fmt[pos].is_ascii_alphabetic() {
            argnum = -1;
        } else {
            argnum = get_argnum(fmt, &mut pos)?;
            loop {
                match fmt.get(pos) {
                    Some(&c @ (b' ' | b'0')) => pad = c,
                    Some(b'-') => left = true,
                    Some(b'+') => always_sign = true,
                    Some(b'\'') => {
                        if fmt.len() - pos > 1 {
                            pos += 1;
                            pad = fmt[pos];
                        } else {
                            return err("ValueError", "Missing padding character");
                        }
                    }
                    _ => break,
                }
                pos += 1;
            }
            // width
            if pos < fmt.len() && fmt[pos] == b'*' {
                pos += 1;
                let mut warg = get_argnum(fmt, &mut pos)?;
                if warg == -1 {
                    warg = currarg;
                    currarg += 1;
                }
                if warg >= argc {
                    if warg > max_missing {
                        max_missing = warg;
                    }
                    continue;
                }
                match &*args[warg as usize].borrow() {
                    Value::Int(n) => {
                        if *n < 0 || *n > i32::MAX as i64 {
                            return err("ValueError", "Width must be between 0 and 2147483647");
                        }
                        width = *n;
                    }
                    _ => return err("ValueError", "Width must be an integer"),
                }
            } else if pos < fmt.len() && fmt[pos].is_ascii_digit() {
                width = get_number(fmt, &mut pos);
                if width < 0 {
                    return err("ValueError", "Width must be between 0 and 2147483647");
                }
            }
            // precision
            if pos < fmt.len() && fmt[pos] == b'.' {
                pos += 1;
                if pos < fmt.len() && fmt[pos] == b'*' {
                    pos += 1;
                    let mut parg = get_argnum(fmt, &mut pos)?;
                    if parg == -1 {
                        parg = currarg;
                        currarg += 1;
                    }
                    if parg >= argc {
                        if parg > max_missing {
                            max_missing = parg;
                        }
                        continue;
                    }
                    match &*args[parg as usize].borrow() {
                        Value::Int(n) => {
                            if *n < -1 || *n > i32::MAX as i64 {
                                return err(
                                    "ValueError",
                                    "Precision must be between -1 and 2147483647",
                                );
                            }
                            precision = *n;
                        }
                        _ => return err("ValueError", "Precision must be an integer"),
                    }
                    adj_prec = true;
                    expprec = true;
                } else if pos < fmt.len() && fmt[pos].is_ascii_digit() {
                    precision = get_number(fmt, &mut pos);
                    if precision < 0 {
                        return err("ValueError", "Precision must be between 0 and 2147483647");
                    }
                    adj_prec = true;
                    expprec = true;
                } else {
                    precision = 0;
                    adj_prec = true;
                }
            }
        }
        // 'l' size modifier is consumed and ignored (%ld works).
        if pos < fmt.len() && fmt[pos] == b'l' {
            pos += 1;
        }
        if argnum == -1 {
            argnum = currarg;
            currarg += 1;
        }
        if argnum >= argc {
            if argnum > max_missing {
                max_missing = argnum;
            }
            continue;
        }
        let spec = if pos < fmt.len() { fmt[pos] } else { 0 };
        if expprec && precision == -1 && !matches!(spec, b'g' | b'G' | b'h' | b'H') {
            return err(
                "ValueError",
                "Precision -1 is only supported for %g, %G, %h and %H",
            );
        }
        let v = args[argnum as usize].borrow().clone();
        let step: Result<(), PhpError> = (|| {
            match spec {
                b's' => append_str(
                    &mut out,
                    &zval_str(it, &v)?,
                    width,
                    precision,
                    pad,
                    left,
                    false,
                    expprec,
                    false,
                ),
                b'd' => append_int(&mut out, zval_long(it, &v)?, width, pad, left, always_sign),
                b'u' => append_uint(&mut out, zval_long(it, &v)? as u64, width, pad, left),
                b'e' | b'E' | b'f' | b'F' | b'g' | b'G' | b'h' | b'H' => {
                    let d = zval_double(it, &v)?;
                    append_double(
                        it,
                        &mut out,
                        d,
                        width,
                        pad,
                        left,
                        precision,
                        adj_prec,
                        spec,
                        always_sign,
                        fname,
                    )
                }
                b'c' => {
                    out.push(zval_long(it, &v)? as u8);
                    Ok(())
                }
                b'o' => append_2n(
                    &mut out,
                    zval_long(it, &v)?,
                    width,
                    pad,
                    left,
                    3,
                    false,
                    expprec,
                ),
                b'x' => append_2n(
                    &mut out,
                    zval_long(it, &v)?,
                    width,
                    pad,
                    left,
                    4,
                    false,
                    expprec,
                ),
                b'X' => append_2n(
                    &mut out,
                    zval_long(it, &v)?,
                    width,
                    pad,
                    left,
                    4,
                    true,
                    expprec,
                ),
                b'b' => append_2n(
                    &mut out,
                    zval_long(it, &v)?,
                    width,
                    pad,
                    left,
                    1,
                    false,
                    expprec,
                ),
                b'%' => {
                    out.push(b'%');
                    Ok(())
                }
                0 if pos >= fmt.len() => {
                    err("ValueError", "Missing format specifier at end of string")
                }
                c => {
                    // Zend interpolates the raw byte into the message
                    // with %c — a NUL specifier truncates the string at
                    // the quote.
                    let msg = if c == 0 {
                        "Unknown format specifier \"".to_string()
                    } else {
                        format!("Unknown format specifier \"{}\"", char::from(c))
                    };
                    err("ValueError", msg)
                }
            }
        })();
        step.map_err(|mut e| {
            if matches!(e.kind, crate::error::ErrorKind::Fatal) {
                if e.line == 0 {
                    e.line = it.cur_line;
                }
                if e.trace.is_none() {
                    e.trace = Some(it.fatal_frames());
                }
            }
            e
        })?;
        pos += 1;
    }
    if max_missing >= 0 {
        if nb == -1 {
            return err(
                "ValueError",
                format!(
                    "The arguments array must contain {} items, {} given",
                    max_missing + 1,
                    argc
                ),
            );
        }
        return err(
            "ArgumentCountError",
            format!(
                "{} arguments are required, {} given",
                max_missing + nb + 1,
                argc + nb
            ),
        );
    }
    Ok(out)
}

/// `php_sprintf_getnumber`: digits parsed by strtol — overflow and
/// negatives collapse to -1.
fn get_number(fmt: &[u8], pos: &mut usize) -> i64 {
    let mut n: i64 = 0;
    while *pos < fmt.len() && fmt[*pos].is_ascii_digit() {
        n = n
            .saturating_mul(10)
            .saturating_add((fmt[*pos] - b'0') as i64);
        *pos += 1;
    }
    if n >= i32::MAX as i64 || n < 0 {
        -1
    } else {
        n
    }
}

/// `php_sprintf_get_argnum`: `N$` positional index (0-based), -1 = next
/// sequential arg. Bad/missing `$` does not consume the digits.
fn get_argnum(fmt: &[u8], pos: &mut usize) -> Result<i64, PhpError> {
    let mut t = *pos;
    while t < fmt.len() && fmt[t].is_ascii_digit() {
        t += 1;
    }
    if t >= fmt.len() || fmt[t] != b'$' {
        return Ok(-1);
    }
    let n = get_number(fmt, pos);
    if n <= 0 {
        return err(
            "ValueError",
            "Argument number specifier must be greater than zero and less than 2147483647",
        );
    }
    *pos += 1; // '$'
    Ok(n - 1)
}

/// `php_sprintf_appendstring` — shared min-width/max-width/pad engine.
#[allow(clippy::too_many_arguments)]
fn append_str(
    out: &mut Vec<u8>,
    add: &[u8],
    min_width: i64,
    max_width: i64,
    padding: u8,
    left: bool,
    neg: bool,
    expprec: bool,
    always_sign: bool,
) -> Result<(), PhpError> {
    let mut copy_len = if expprec {
        (max_width.max(0) as usize).min(add.len())
    } else {
        add.len()
    };
    let m_width = (min_width.max(0)).max(copy_len as i64) as u64;
    if m_width > i32::MAX as u64 - out.len() as u64 - 1 {
        return Err(PhpError::fatal(
            format!("Field width {} is too long", m_width),
            0,
        ));
    }
    let npad = (min_width as usize).saturating_sub(copy_len);
    let mut body = add;
    if !left {
        if (neg || always_sign) && padding == b'0' {
            out.push(if neg { b'-' } else { b'+' });
            body = &body[1..];
            copy_len -= 1;
        }
        out.extend(std::iter::repeat_n(padding, npad));
    }
    out.extend_from_slice(&body[..copy_len]);
    if left {
        out.extend(std::iter::repeat_n(padding, npad));
    }
    Ok(())
}

/// `php_sprintf_appendint` — decimal digits, sign-aware '0' padding.
fn append_int(
    out: &mut Vec<u8>,
    n: i64,
    width: i64,
    padding: u8,
    left: bool,
    always_sign: bool,
) -> Result<(), PhpError> {
    let neg = n < 0;
    // Can't right-pad 0's on integers (zend converts to space).
    let padding = if left && padding == b'0' {
        b' '
    } else {
        padding
    };
    let mut body = String::new();
    if neg {
        body.push('-');
    } else if always_sign {
        body.push('+');
    }
    body.push_str(&n.unsigned_abs().to_string());
    append_str(
        out,
        body.as_bytes(),
        width,
        0,
        padding,
        left,
        neg,
        false,
        always_sign,
    )
}

/// `php_sprintf_appenduint`.
fn append_uint(
    out: &mut Vec<u8>,
    n: u64,
    width: i64,
    padding: u8,
    left: bool,
) -> Result<(), PhpError> {
    let padding = if left && padding == b'0' {
        b' '
    } else {
        padding
    };
    append_str(
        out,
        n.to_string().as_bytes(),
        width,
        0,
        padding,
        left,
        false,
        false,
        false,
    )
}

/// `php_sprintf_append2n` — base-2^n unsigned conversion (o/x/X/b).
#[allow(clippy::too_many_arguments)]
fn append_2n(
    out: &mut Vec<u8>,
    n: i64,
    width: i64,
    padding: u8,
    left: bool,
    nbits: u32,
    upper: bool,
    expprec: bool,
) -> Result<(), PhpError> {
    let digits = match (nbits, upper) {
        (3, _) => format!("{:o}", n as u64),
        (4, false) => format!("{:x}", n as u64),
        (4, true) => format!("{:X}", n as u64),
        _ => format!("{:b}", n as u64),
    };
    append_str(
        out,
        digits.as_bytes(),
        width,
        0,
        padding,
        left,
        false,
        expprec,
        false,
    )
}

/// `php_sprintf_appenddouble` — NaN/INF bypass width; %e/%f go through
/// conv_fp semantics, %g/%G/%h/%H through zend_gcvt.
#[allow(clippy::too_many_arguments)]
fn append_double(
    it: &mut Interp,
    out: &mut Vec<u8>,
    f: f64,
    width: i64,
    padding: u8,
    left: bool,
    precision: i64,
    adj_prec: bool,
    fmt: u8,
    always_sign: bool,
    fname: &str,
) -> Result<(), PhpError> {
    let mut precision = precision;
    if !adj_prec {
        precision = 6;
    } else if precision > 53 {
        it.notice_pub(&format!(
            "{}(): Requested precision of {} digits was truncated to PHP maximum of 53 digits",
            fname, precision
        ))?;
        precision = 53;
    }
    if f.is_nan() {
        return append_str(out, b"NaN", 3, 0, padding, left, false, false, always_sign);
    }
    if f.is_infinite() {
        let neg = f < 0.0;
        let body: &[u8] = if neg { b"-INF" } else { b"INF" };
        return append_str(
            out,
            body,
            body.len() as i64,
            0,
            padding,
            left,
            neg,
            false,
            always_sign,
        );
    }
    let prec = precision.clamp(0, i64::MAX) as usize;
    let (neg, body) = match fmt {
        b'e' | b'E' => (f < 0.0, conv_e(f, prec, fmt == b'E')),
        b'f' | b'F' => (f < 0.0, format!("{:.*}", prec, f.abs())),
        // 'g' 'G' 'h' 'H'
        _ => {
            let ndigit = if precision == 0 { 1 } else { precision } as i32;
            let exp = if fmt == b'G' || fmt == b'H' {
                b'E'
            } else {
                b'e'
            };
            zend_gcvt(f, ndigit, exp)
        }
    };
    let mut s = Vec::with_capacity(body.len() + 1);
    if neg {
        s.push(b'-');
    } else if always_sign {
        s.push(b'+');
    }
    s.extend_from_slice(body.as_bytes());
    append_str(out, &s, width, 0, padding, left, neg, false, always_sign)
}

/// `php_conv_fp` for 'e'/'E': mantissa via mode-2 dtoa equivalent
/// (`{:.*e}` gives the same correctly-rounded digits), exponent printed
/// with a sign and NO zero padding ("e+4", not "e+04").
fn conv_e(f: f64, prec: usize, upper: bool) -> String {
    let s = format!("{:.*e}", prec, f.abs());
    let (mant, exp) = s.split_once('e').unwrap();
    let exp: i64 = exp.parse().unwrap_or(0);
    let e = if upper { 'E' } else { 'e' };
    if exp < 0 {
        format!("{}{}-{}", mant, e, -exp)
    } else {
        format!("{}{}+{}", mant, e, exp)
    }
}

/// `zend_gcvt` — returns (is_negative, digits-with-point). Mode 2 digits
/// (`{:.*e}` minus trailing zeros); mode 0 (precision -1) = shortest
/// repr digits.
fn zend_gcvt(f: f64, ndigit: i32, exp_char: u8) -> (bool, String) {
    let sign = f.is_sign_negative();
    let a = f.abs();
    let (digits, decpt) = if ndigit < 0 {
        // dtoa mode 0 — shortest round-trip
        let s = format!("{:e}", a);
        let (mant, exp) = s.split_once('e').unwrap();
        let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
        let digits = digits.trim_end_matches('0');
        let digits = if digits.is_empty() { "0" } else { digits };
        (digits.to_string(), exp.parse::<i64>().unwrap_or(0) + 1)
    } else {
        // dtoa mode 2 — ndigit significant digits, trailing zeros stripped
        let s = format!("{:.*e}", (ndigit - 1).max(0) as usize, a);
        let (mant, exp) = s.split_once('e').unwrap();
        let raw: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
        let digits = raw.trim_end_matches('0');
        let digits = if digits.is_empty() { "0" } else { digits };
        (digits.to_string(), exp.parse::<i64>().unwrap_or(0) + 1)
    };
    let dp = b'.';
    let mut dst = String::new();
    let nd = if ndigit < 0 { 17 } else { ndigit } as i64;
    if (decpt >= 0 && decpt > nd) || decpt < -3 {
        // E-style
        let exp = decpt - 1;
        dst.push(digits.as_bytes()[0] as char);
        dst.push(dp as char);
        if digits.len() > 1 {
            dst.push_str(&digits[1..]);
        } else {
            dst.push('0');
        }
        dst.push(exp_char as char);
        dst.push(if exp < 0 { '-' } else { '+' });
        dst.push_str(&exp.unsigned_abs().to_string());
    } else if decpt < 0 {
        dst.push('0');
        dst.push(dp as char);
        for _ in 0..(-decpt) {
            dst.push('0');
        }
        dst.push_str(&digits);
    } else {
        // standard
        let ds = digits.as_bytes();
        for i in 0..decpt {
            if (i as usize) < ds.len() {
                dst.push(ds[i as usize] as char);
            } else {
                dst.push('0');
            }
        }
        if (decpt as usize) < ds.len() {
            if decpt == 0 {
                dst.push('0');
            }
            dst.push(dp as char);
            dst.push_str(&digits[decpt as usize..]);
        }
    }
    (sign, dst)
}

/// `zval_get_tmp_string` for %s — arrays warn, objects go through
/// __toString; a failed cast raises the pending Error.
fn zval_str(it: &mut Interp, v: &Value) -> Result<Vec<u8>, PhpError> {
    match v {
        Value::Str(s) => Ok(s.to_vec()),
        _ => match it.try_conv_bytes(v) {
            Ok(b) => Ok(b),
            Err(e) => {
                if e.kind == crate::error::ErrorKind::Throw {
                    if let Some(x) = it.take_pending_exception() {
                        // Zend drops the innermost internal frame
                        // when the call was compile-specialized away
                        // (const-format literal `sprintf` → rope-concat
                        // — sprintf_rope_optimization_002); every real
                        // call keeps it. PHP frames above the internal
                        // one don't change that.
                        if let Value::Object(o) = &x {
                            if let Some(crate::value::ObjectInternal::Exception {
                                frames, ..
                            }) = &mut o.borrow_mut().internal
                            {
                                if let Some(pos) = frames.iter().rposition(|fr| fr.internal) {
                                    if !frames[pos].visible {
                                        let mut f = (**frames).clone();
                                        f.remove(pos);
                                        *frames = Rc::new(f);
                                    }
                                }
                            }
                        }
                        return Err(it.throw_value(x));
                    }
                }
                Err(e)
            }
        },
    }
}

/// `zval_get_long` — floats warn+wrap (zend_dtoi64), objects warn.
fn zval_long(it: &mut Interp, v: &Value) -> Result<i64, PhpError> {
    Ok(match v {
        Value::Float(f) => {
            const MOD: f64 = 18446744073709551616.0; // 2^64
            if !f.is_finite() {
                it.warn_pub(&format!(
                    "The float {} is not representable as an int, cast occurred",
                    format_float_repr(*f)
                ))?;
                0
            } else if *f >= i64::MAX as f64 || *f < i64::MIN as f64 {
                it.warn_pub(&format!(
                    "The float {} is not representable as an int, cast occurred",
                    format_float_repr(*f)
                ))?;
                let m = f % MOD;
                let u = if m < 0.0 { m + MOD } else { m };
                u as u64 as i64
            } else {
                *f as i64
            }
        }
        Value::Object(o) => {
            let cn = o.borrow().class.name().to_string();
            it.warn_pub(&format!(
                "Object of class {} could not be converted to int",
                cn
            ))?;
            1
        }
        Value::Callable(_) => {
            it.warn_pub("Object of class Closure could not be converted to int")?;
            1
        }
        _ => v.to_int(),
    })
}

/// `zval_get_double`.
fn zval_double(it: &mut Interp, v: &Value) -> Result<f64, PhpError> {
    Ok(match v {
        Value::Object(o) => {
            let cn = o.borrow().class.name().to_string();
            it.warn_pub(&format!(
                "Object of class {} could not be converted to float",
                cn
            ))?;
            1.0
        }
        Value::Callable(_) => {
            it.warn_pub("Object of class Closure could not be converted to float")?;
            1.0
        }
        _ => v.to_float(),
    })
}
