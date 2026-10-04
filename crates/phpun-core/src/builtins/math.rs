//! Math builtins: numeric fns, rand, integer bases.

use super::*;

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
    Ok(Some(match name {
        // ----- math -----
        "abs" => match arg(args, 0) {
            Value::Int(i) => Value::Int(i.saturating_abs()),
            v => Value::Float(v.to_float().abs()),
        },
        "intdiv" => {
            let (va, vb) = (arg(args, 0), arg(args, 1));
            let a = it.coerce_int_pub(&va);
            let b = it.coerce_int_pub(&vb);
            if b == 0 {
                return err("DivisionByZeroError", "Division by zero");
            }
            match a.checked_div(b) {
                Some(q) => Value::Int(q),
                None => {
                    return err(
                        "ArithmeticError",
                        "Division of PHP_INT_MIN by -1 is not an integer",
                    )
                }
            }
        }
        "divmod" => Value::Null,
        "max" | "min" => {
            let mut vals: Vec<Value> = Vec::new();
            for a in args {
                let v = a.borrow().clone();
                if let Value::Array(arr) = &v {
                    for (_, c) in arr.borrow().iter() {
                        vals.push(c.borrow().clone());
                    }
                } else {
                    vals.push(v);
                }
            }
            let mut best = vals.first().cloned().unwrap_or(Value::Null);
            for v in &vals[1.min(vals.len())..] {
                let ord = compare(v, &best);
                if (name == "max" && ord == std::cmp::Ordering::Greater)
                    || (name == "min" && ord == std::cmp::Ordering::Less)
                {
                    best = v.clone();
                }
            }
            best
        }
        "round" => {
            let v = arg(args, 0).to_float();
            let p = arg(args, 1).to_int();
            let m = 10f64.powi(p as i32);
            let _mode = arg(args, 2).to_int();
            let r = v * m;
            let rounded = if r >= 0.0 {
                (r + 0.5).floor()
            } else {
                (r - 0.5).ceil()
            };
            Value::Float(rounded / m)
        }
        "floor" => Value::Float(arg(args, 0).to_float().floor()),
        "ceil" => Value::Float(arg(args, 0).to_float().ceil()),
        "fmod" => Value::Float(arg(args, 0).to_float() % arg(args, 1).to_float()),
        "fdiv" => {
            let d = arg(args, 1).to_float();
            if d == 0.0 {
                let n = arg(args, 0).to_float();
                Value::Float(if n > 0.0 {
                    f64::INFINITY
                } else if n < 0.0 {
                    f64::NEG_INFINITY
                } else {
                    f64::NAN
                })
            } else {
                Value::Float(arg(args, 0).to_float() / d)
            }
        }
        "pow" => {
            let b = arg(args, 0);
            let e = arg(args, 1);
            match (&b, &e) {
                (Value::Int(bi), Value::Int(ei)) if *ei >= 0 => match bi.checked_pow(*ei as u32) {
                    Some(v) => Value::Int(v),
                    None => Value::Float(b.to_float().powf(e.to_float())),
                },
                _ => Value::Float(b.to_float().powf(e.to_float())),
            }
        }
        "sqrt" => Value::Float(arg(args, 0).to_float().sqrt()),
        "exp" => Value::Float(arg(args, 0).to_float().exp()),
        "log" | "ln" => {
            let v = arg(args, 0).to_float();
            if args.len() > 1 {
                Value::Float(v.ln() / arg(args, 1).to_float().ln())
            } else {
                Value::Float(v.ln())
            }
        }
        "log10" => Value::Float(arg(args, 0).to_float().log10()),
        "log2" => Value::Float(arg(args, 0).to_float().log2()),
        "sin" => Value::Float(arg(args, 0).to_float().sin()),
        "cos" => Value::Float(arg(args, 0).to_float().cos()),
        "tan" => Value::Float(arg(args, 0).to_float().tan()),
        "asin" => Value::Float(arg(args, 0).to_float().asin()),
        "acos" => Value::Float(arg(args, 0).to_float().acos()),
        "atan" => Value::Float(arg(args, 0).to_float().atan()),
        "atan2" => Value::Float(arg(args, 0).to_float().atan2(arg(args, 1).to_float())),
        "sinh" => Value::Float(arg(args, 0).to_float().sinh()),
        "cosh" => Value::Float(arg(args, 0).to_float().cosh()),
        "tanh" => Value::Float(arg(args, 0).to_float().tanh()),
        "pi" => Value::Float(std::f64::consts::PI),
        "deg2rad" => Value::Float(arg(args, 0).to_float() * std::f64::consts::PI / 180.0),
        "rad2deg" => Value::Float(arg(args, 0).to_float() * 180.0 / std::f64::consts::PI),
        "hypot" => Value::Float(arg(args, 0).to_float().hypot(arg(args, 1).to_float())),
        "rand" | "mt_rand" | "random_int" => {
            use std::time::{SystemTime, UNIX_EPOCH};
            let lo = arg(args, 0).to_int();
            let hi = arg(args, 1).to_int();
            let seed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
                .unwrap_or(0x9e3779b9);
            let r = xorshift(seed);
            Value::Int(if hi >= lo {
                lo + (r % ((hi - lo + 1) as u64).max(1)) as i64
            } else {
                lo
            })
        }
        "mt_srand" | "srand" => Value::Null,
        "mt_getrandmax" | "getrandmax" => Value::Int(2147483647),
        "lcg_value" => Value::Float(0.5),
        "decbin" => Value::str(format!("{:b}", arg(args, 0).to_int())),
        "dechex" => Value::str(format!("{:x}", arg(args, 0).to_int())),
        "decoct" => Value::str(format!("{:o}", arg(args, 0).to_int())),
        "bindec" => Value::Int(i64::from_str_radix(&arg_str(it, args, 0), 2).unwrap_or(0)),
        "hexdec" => {
            let s = arg_str(it, args, 0);
            match i64::from_str_radix(s.trim(), 16) {
                Ok(v) => Value::Int(v),
                Err(_) => match u64::from_str_radix(s.trim(), 16) {
                    Ok(v) => Value::Float(v as f64),
                    Err(_) => Value::Int(0),
                },
            }
        }
        "octdec" => Value::Int(i64::from_str_radix(&arg_str(it, args, 0), 8).unwrap_or(0)),
        "base_convert" => {
            let s = arg_str(it, args, 0);
            let from = arg(args, 1).to_int() as u32;
            let to = arg(args, 2).to_int() as u32;
            match i128::from_str_radix(&s.to_lowercase(), from) {
                Ok(v) => Value::str(base_conv(v, to)),
                Err(_) => Value::Bool(false),
            }
        }
        _ => return Ok(None),
    }))
}

// ----- helpers -----

fn xorshift(mut x: u64) -> u64 {
    if x == 0 {
        x = 0x9e3779b97f4a7c15;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}

fn base_conv(mut v: i128, to: u32) -> String {
    if v == 0 {
        return "0".into();
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    let neg = v < 0;
    if neg {
        v = -v;
    }
    while v > 0 {
        out.push(digits[(v % to as i128) as usize] as char);
        v /= to as i128;
    }
    if neg {
        out.push('-');
    }
    out.iter().rev().collect()
}
