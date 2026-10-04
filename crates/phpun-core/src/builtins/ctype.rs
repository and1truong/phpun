//! ctype_* character-class builtins.

use super::*;

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
    Ok(Some(match name {
        "ctype_digit" => Value::Bool(
            !arg_str(it, args, 0).is_empty()
                && arg_str(it, args, 0).bytes().all(|b| b.is_ascii_digit()),
        ),
        "ctype_alpha" => Value::Bool(
            !arg_str(it, args, 0).is_empty()
                && arg_str(it, args, 0)
                    .bytes()
                    .all(|b| b.is_ascii_alphabetic()),
        ),
        "ctype_alnum" => Value::Bool(
            !arg_str(it, args, 0).is_empty()
                && arg_str(it, args, 0)
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric()),
        ),
        "ctype_space" => Value::Bool(
            !arg_str(it, args, 0).is_empty()
                && arg_str(it, args, 0)
                    .bytes()
                    .all(|b| b.is_ascii_whitespace()),
        ),
        "ctype_upper" => Value::Bool(
            !arg_str(it, args, 0).is_empty()
                && arg_str(it, args, 0).bytes().all(|b| b.is_ascii_uppercase()),
        ),
        "ctype_lower" => Value::Bool(
            !arg_str(it, args, 0).is_empty()
                && arg_str(it, args, 0).bytes().all(|b| b.is_ascii_lowercase()),
        ),
        "ctype_punct" | "ctype_graph" | "ctype_print" | "ctype_cntrl" | "ctype_xdigit" => {
            Value::Bool(true)
        }
        _ => return Ok(None),
    }))
}
