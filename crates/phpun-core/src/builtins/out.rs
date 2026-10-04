//! Formatted output (printf family) and output-buffering (ob_*) builtins.

use super::fs::write_resource;
use super::*;

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
    Ok(Some(match name {
        "printf" => {
            let s = sprintf(it, args)?;
            it.emit(&s);
            Value::Int(s.len() as i64)
        }
        "sprintf" => Value::str(sprintf(it, args)?),
        "vsprintf" => {
            let fmt = arg_str(it, args, 0);
            let list = match arg(args, 1) {
                Value::Array(a) => a
                    .borrow()
                    .entries
                    .iter()
                    .map(|(_, c)| c.clone())
                    .collect::<Vec<_>>(),
                _ => vec![],
            };
            Value::str(sprintf_args(it, &fmt, &list)?)
        }
        "vprintf" => {
            let fmt = arg_str(it, args, 0);
            let list = match arg(args, 1) {
                Value::Array(a) => a
                    .borrow()
                    .entries
                    .iter()
                    .map(|(_, c)| c.clone())
                    .collect::<Vec<_>>(),
                _ => vec![],
            };
            let s = sprintf_args(it, &fmt, &list)?;
            it.emit(&s);
            Value::Int(s.len() as i64)
        }
        "fprintf" => {
            // fprintf($fh, fmt, ...) — write to resource
            let fmt = arg_str(it, args, 1);
            let s = sprintf_args(it, &fmt, &args[2.min(args.len())..])?;
            write_resource(it, args.first(), s.as_bytes())?;
            Value::Int(s.len() as i64)
        }
        "sprintf_js" | "vsprintf_js" => Value::Null,

        // ----- output buffering -----
        "ob_start" => {
            let h = args.first().map(|c| c.borrow().clone());
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

fn sprintf(it: &mut Interp, args: &[Cell]) -> Result<String, PhpError> {
    let fmt = arg_str(it, args, 0);
    sprintf_args(it, &fmt, &args[1.min(args.len())..])
}

fn sprintf_args(it: &mut Interp, fmt: &str, args: &[Cell]) -> Result<String, PhpError> {
    let mut out = String::new();
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    let mut argi = 0usize;
    while i < chars.len() {
        if chars[i] != '%' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= chars.len() {
            break;
        }
        if chars[i] == '%' {
            out.push('%');
            i += 1;
            continue;
        }
        // argnum$
        let mut idx_override = None;
        let save = i;
        while i < chars.len() && chars[i].is_ascii_digit() {
            i += 1;
        }
        if i < chars.len() && chars[i] == '$' {
            idx_override = Some(
                chars[save..i]
                    .iter()
                    .collect::<String>()
                    .parse::<usize>()
                    .unwrap_or(1)
                    - 1,
            );
            i += 1;
        } else {
            i = save;
        }
        // flags
        let mut pad = ' ';
        let mut left = false;
        let mut plus = false;
        let mut alt = false;
        loop {
            match chars.get(i) {
                Some('0') => pad = '0',
                Some('-') => left = true,
                Some('+') => plus = true,
                Some(' ') => {}
                Some('#') => alt = true,
                Some('\'') => {
                    i += 1;
                    pad = chars.get(i).copied().unwrap_or(' ');
                }
                _ => break,
            }
            i += 1;
        }
        // width
        let mut width = String::new();
        while i < chars.len() && chars[i].is_ascii_digit() {
            width.push(chars[i]);
            i += 1;
        }
        let width: usize = width.parse().unwrap_or(0);
        // precision
        let mut prec: Option<usize> = None;
        if i < chars.len() && chars[i] == '.' {
            i += 1;
            let mut p = String::new();
            while i < chars.len() && chars[i].is_ascii_digit() {
                p.push(chars[i]);
                i += 1;
            }
            prec = Some(p.parse().unwrap_or(0));
        }
        if i >= chars.len() {
            break;
        }
        let spec = chars[i];
        i += 1;
        let ai = idx_override.unwrap_or(argi);
        argi = ai + 1;
        let v = args
            .get(ai)
            .map(|c| c.borrow().clone())
            .unwrap_or(Value::Null);
        let body = fmt_spec(it, spec, &v, prec, alt)?;
        let signed = plus && matches!(spec, 'd' | 'i' | 'e' | 'E' | 'f' | 'F' | 'g' | 'G');
        let body = if signed && !body.starts_with('-') {
            format!("+{}", body)
        } else {
            body
        };
        if body.len() < width {
            let fill = width - body.len();
            let mut padded = String::new();
            if left {
                padded.push_str(&body);
                padded.push_str(&" ".repeat(fill));
            } else if pad == '0' && body.starts_with('-') {
                padded.push('-');
                padded.push_str(&"0".repeat(fill));
                padded.push_str(&body[1..]);
            } else if pad == '0' && body.starts_with('+') {
                padded.push('+');
                padded.push_str(&"0".repeat(fill));
                padded.push_str(&body[1..]);
            } else {
                padded.push_str(&pad.to_string().repeat(fill));
                padded.push_str(&body);
            }
            out.push_str(&padded);
        } else {
            out.push_str(&body);
        }
    }
    Ok(out)
}

fn fmt_spec(
    it: &mut Interp,
    spec: char,
    v: &Value,
    prec: Option<usize>,
    alt: bool,
) -> Result<String, PhpError> {
    Ok(match spec {
        's' | 'S' => {
            let s = it.to_string_of(v);
            match prec {
                Some(p) => s.chars().take(p).collect(),
                None => s,
            }
        }
        'd' | 'i' | 'u' => v.to_int().to_string(),
        'b' => format!("{:b}", v.to_int()),
        'o' => format!("{:o}", v.to_int()),
        'x' => format!("{:x}", v.to_int()),
        'X' => format!("{:X}", v.to_int()),
        'c' => String::from_utf8_lossy(&[(v.to_int() & 0xff) as u8]).into_owned(),
        'e' => format!("{:.*e}", prec.unwrap_or(6), v.to_float()),
        'E' => format!("{:.*E}", prec.unwrap_or(6), v.to_float())
            .replace("e+", "E+")
            .replace("e-", "E-"),
        'f' | 'F' => format!("{:.*}", prec.unwrap_or(6), v.to_float()),
        'g' | 'G' => {
            let p = prec.unwrap_or(6).max(1);
            let s = crate::value::gcvt(v.to_float(), p);
            if spec == 'g' {
                s.to_lowercase()
            } else {
                s
            }
        }
        'h' | 'H' => crate::value::gcvt(v.to_float(), prec.unwrap_or(6).max(1)),
        'a' | 'A' => v.to_float().to_string(),
        'r' => v.to_php_string(),
        'v' => crate::value::format_float(v.to_float()),
        '%' => "%".into(),
        _ => {
            let _ = alt;
            format!("%{}", spec)
        }
    })
}
