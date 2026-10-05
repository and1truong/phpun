//! Free helpers shared by the `interp` submodules: cell/key
//! plumbing, weak-mode scalar coercion, type-name rendering and
//! the const-initializer predicate.

use super::*;

/// Hooks merged along a chain (hook, declaring class), nearest first.
pub type MergedHooks = Vec<(PropHook, Rc<PhpClass>)>;
/// `(emitted key, slot key, decl+decl class)` — `None` decl means a
/// dynamic property (property-hooks serialization views).
pub type SerialEntry = (String, String, Option<(PropDecl, Rc<PhpClass>)>);
/// `(name, cell)` entries a `...$v` unpack yields — `None` name is
/// positional.
pub type SpreadItems = Vec<(Option<Rc<str>>, Cell)>;

pub(in crate::interp) fn cell(v: Value) -> Cell {
    Rc::new(RefCell::new(v))
}

pub(in crate::interp) fn key_value(k: &ArrKey) -> Value {
    match k {
        ArrKey::Int(i) => Value::Int(*i),
        ArrKey::Str(s) => Value::str(s.to_string()),
        ArrKey::Tomb => Value::Null,
    }
}

/// Weak-mode scalar coercion used by typed-property writes and hook
/// type checks ("C::$p: Return value must be of type int" family).
pub(in crate::interp) fn weak_ty_coerce(tys: &[String], v: &Value) -> Option<Value> {
    for t in tys {
        let coerced = match (t.as_str(), v) {
            ("int", Value::Str(s)) => {
                let tr = crate::value::lossy(s);
                let tr = tr.trim();
                let base = if let Some(h) = tr.strip_prefix("0x") {
                    i64::from_str_radix(h, 16).ok()
                } else if let Some(o) = tr.strip_prefix("0o") {
                    i64::from_str_radix(o, 8).ok()
                } else if let Some(b) = tr.strip_prefix("0b") {
                    i64::from_str_radix(b, 2).ok()
                } else {
                    tr.parse::<i64>().ok()
                };
                base.map(Value::Int)
            }
            ("int", Value::Float(f)) => {
                // Zend refuses out-of-range float->int coercions
                // (NaN/Inf/|f| >= 2^63 → TypeError, no saturation).
                if f.is_finite() && *f < 9.223372036854776e18 && *f >= -9.223372036854776e18 {
                    Some(Value::Int(*f as i64))
                } else {
                    None
                }
            }
            ("int", Value::Bool(b)) => Some(Value::Int(*b as i64)),
            ("string", Value::Int(i)) => Some(Value::str(i.to_string())),
            ("string", Value::Float(f)) => Some(Value::str(format_float_repr(*f))),
            ("string", Value::Bool(b)) => Some(Value::str(if *b { "1" } else { "" })),
            ("float", Value::Int(i)) => Some(Value::Float(*i as f64)),
            ("float", Value::Str(s)) => crate::value::lossy(s)
                .trim()
                .parse::<f64>()
                .ok()
                .map(Value::Float),
            ("float", Value::Bool(b)) => Some(Value::Float(if *b { 1.0 } else { 0.0 })),
            ("bool", _) => Some(Value::Bool(v.is_truthy())),
            _ => None,
        };
        if let Some(c) = coerced {
            return Some(c);
        }
    }
    None
}

/// Render a parsed type member list the way Zend prints it — a union
/// containing `null` displays as `?T`.
pub(in crate::interp) fn ty_disp(ty: &[String]) -> String {
    let mut nullable = false;
    let mut rest: Vec<String> = Vec::new();
    for m in ty {
        if m.eq_ignore_ascii_case("null") {
            nullable = true;
        } else {
            rest.push(
                m.split('&')
                    .map(|p| {
                        let p = p.trim_start_matches('\\');
                        if let Some(pos) = p.find("@anonymous$") {
                            format!("{}@anonymous", &p[..pos])
                        } else {
                            p.to_string()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("&"),
            );
        }
    }
    let joined = if rest.len() > 1 || nullable {
        rest.iter()
            .map(|m| {
                if m.contains('&') {
                    format!("({m})")
                } else {
                    m.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("|")
    } else {
        rest.join("|")
    };
    if nullable && rest.is_empty() {
        "null".to_string()
    } else if nullable && rest.len() == 1 && !rest[0].contains('&') {
        format!("?{}", joined)
    } else if nullable {
        // `(X&Y)|null` — intersections can't take the ? shortcut.
        format!("{}|null", joined)
    } else {
        joined
    }
}

/// Whether a const initializer is a pure compile-time expression
/// (literals and operators over them — no fetches, calls, `new`).
/// Only these get Zend's eager "Cannot use ... as value" fatal at
/// class registration; everything else type-checks lazily at access.
pub(in crate::interp) fn is_compile_const(e: &Expr) -> bool {
    match e {
        Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Float(_) | Expr::Str(_) => true,
        Expr::Interp(parts) => parts
            .iter()
            .all(|p| matches!(p, crate::lexer::StringPart::Lit(_))),
        Expr::ArrayLit(items) => items.iter().all(|(_, v)| is_compile_const(v)),
        Expr::Unary { e, .. } => is_compile_const(e),
        Expr::Binary { l, r, .. } => is_compile_const(l) && is_compile_const(r),
        Expr::Cast { e, .. } => is_compile_const(e),
        Expr::Ternary { c, t, f } => {
            is_compile_const(c)
                && t.as_ref().map(|x| is_compile_const(x)).unwrap_or(true)
                && is_compile_const(f)
        }
        _ => false,
    }
}

/// The znode op_type zend's pass_two assigns each operand shape:
/// IS_CONST(1) < IS_TMP_VAR(2) < IS_VAR(4) < IS_CV(8). `Expr::Var` is the
/// only CV-grade operand; calls/`new` produce VAR; compile-time
/// constants rank IS_CONST; every other expression (fetches, unary/
/// binary ops, assignments, `??`, ternaries) is IS_TMP_VAR. Used by the
/// COMMUTATIVE canonicalization of the equality ops (see compare_op).
pub(in crate::interp) fn compare_operand_rank(e: &Expr) -> u8 {
    match e {
        Expr::Var(_) => 8,
        // zend strips parens during compilation — `($x)` is the CV.
        Expr::Paren(e) => compare_operand_rank(e),
        Expr::Call { .. }
        | Expr::MethodCall { .. }
        | Expr::StaticCall { .. }
        | Expr::StaticCallDyn { .. }
        | Expr::New { .. }
        | Expr::AnonClass(_)
        // ZEND_INCLUDE_OR_EVAL and ZEND_YIELD emit their result into a
        // real znode (zend_emit_op, not _tmp) → IS_VAR. YIELD_FROM and
        // closure literals are _tmp emits → IS_TMP_VAR.
        | Expr::Include { .. }
        | Expr::Yield { .. } => 4,
        // BEGIN_SILENCE is rank-transparent: `@expr` keeps the inner
        // znode's type — except `@$var`, which zend forces through
        // zend_compile_simple_var_no_cv (FETCH_R → IS_TMP_VAR) so the
        // CV read happens inside the silenced section.
        Expr::Unary { op: "@", e } => {
            let mut inner = &**e;
            while let Expr::Paren(p) = inner {
                inner = p;
            }
            match inner {
                Expr::Var(_) => 2,
                _ => compare_operand_rank(inner),
            }
        }
        _ if is_compile_const(e) => 1,
        _ => 2,
    }
}
