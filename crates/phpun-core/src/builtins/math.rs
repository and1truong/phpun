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
            let a = intdiv_int(it, &va, 1, "num1")?;
            let b = intdiv_int(it, &vb, 2, "num2")?;
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
            // zend's compare direction depends on the call shape — the
            // LEFT operand is the side the cyclic-compare check
            // protects:
            //   min($arr)     — zend_hash_minmax does compar(res, zv):
            //                   LEFT = running best (earlier element).
            //   min($a, ...)  — PHP_FUNCTION does zend_compare(args[i],
            //                   best): LEFT = each later arg. Array
            //                   args are NOT flattened here.
            //   min($a, $b)   — a direct call compiles to the frameless
            //                   fast path zend_compare(lhs, rhs): LEFT
            //                   = arg 1 — handled in call_named.
            if args.is_empty() {
                return Err(PhpError::uncaught(
                    "ArgumentCountError",
                    format!("{}() expects at least 1 argument, 0 given", name),
                    0,
                ));
            }
            if args.len() == 1 {
                let v = arg(args, 0);
                let Value::Array(arr) = &v else {
                    return Err(PhpError::uncaught(
                        "TypeError",
                        format!(
                            "{}(): Argument #1 ($value) must be of type array, {} given",
                            name,
                            zval_word(&v)
                        ),
                        0,
                    ));
                };
                let mut best: Option<Value> = None;
                // A stale CMP_DEPTH_ERR from an earlier caught Error
                // must not bleed into this builtin's flag reads.
                crate::value::clear_cmp_depth_err();
                for (_, c) in arr.borrow().iter() {
                    let v = c.borrow().clone();
                    let Some(b) = &best else {
                        best = Some(v);
                        continue;
                    };
                    let ord = compare(b, &v);
                    if crate::value::cmp_depth_err() {
                        return depth_err();
                    }
                    if (name == "min" && ord == std::cmp::Ordering::Greater)
                        || (name == "max" && ord == std::cmp::Ordering::Less)
                    {
                        best = Some(v);
                    }
                }
                match best {
                    Some(v) => v,
                    None => {
                        return Err(PhpError::uncaught(
                            "ValueError",
                            format!(
                                "{}(): Argument #1 ($value) must contain at least one element",
                                name
                            ),
                            0,
                        ));
                    }
                }
            } else {
                let mut best = arg(args, 0);
                crate::value::clear_cmp_depth_err();
                for a in &args[1..] {
                    let v = a.borrow().clone();
                    let ord = compare(&v, &best);
                    if crate::value::cmp_depth_err() {
                        return depth_err();
                    }
                    if (name == "min" && ord == std::cmp::Ordering::Less)
                        || (name == "max" && ord == std::cmp::Ordering::Greater)
                    {
                        best = v;
                    }
                }
                best
            }
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
            // OptReq stub is all-or-none — zero args = full mt range
            // (partial binding is rejected in resolve_named_builtin).
            let (lo, hi) = if args.is_empty() {
                (0, 2147483647)
            } else {
                (arg(args, 0).to_int(), arg(args, 1).to_int())
            };
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

/// intdiv()'s zpp 'l' arg coercion: bools and int strings coerce
/// silently, in-range fractional floats deprecate, out-of-range floats
/// and non-numeric types are the arginfo TypeError, null deprecates.
fn intdiv_int(it: &mut Interp, v: &Value, n: usize, name: &str) -> Result<i64, PhpError> {
    let te = |tn: &str| -> PhpError {
        err::<i64>(
            "TypeError",
            format!(
                "intdiv(): Argument #{} (${}) must be of type int, {} given",
                n, name, tn
            ),
        )
        .unwrap_err()
    };
    match v {
        Value::Float(f) => {
            if !f.is_finite() || *f >= i64::MAX as f64 || *f < i64::MIN as f64 {
                return Err(te("float"));
            }
            if f.fract() != 0.0 {
                it.deprecated_pub(&format!(
                    "Implicit conversion from float {} to int loses precision",
                    crate::value::format_float_repr(*f)
                ))?;
            }
            Ok(*f as i64)
        }
        Value::Str(s) => match crate::value::numeric(s) {
            crate::value::Numeric::Int(i) => Ok(i),
            crate::value::Numeric::Float(f) => {
                if !f.is_finite() || f >= i64::MAX as f64 || f < i64::MIN as f64 {
                    return Err(te("string"));
                }
                if f.fract() != 0.0 {
                    it.deprecated_pub(&format!(
                        "Implicit conversion from float-string \"{}\" to int loses precision",
                        String::from_utf8_lossy(s)
                    ))?;
                }
                Ok(f as i64)
            }
            _ => Err(te("string")),
        },
        Value::Null => Ok(0),
        Value::Int(i) => Ok(*i),
        Value::Bool(b) => Ok(*b as i64),
        other => Err(te(&other.operand_type_name())),
    }
}
