//! SPL builtins: spl_autoload_*, spl_object_*, iterator_*.

use super::*;

pub(crate) fn dispatch(
    it: &mut Interp,
    name: &str,
    args: &[Cell],
) -> Result<Option<Value>, PhpError> {
    Ok(Some(match name {
        "spl_object_id" | "spl_object_hash" => match arg(args, 0) {
            Value::Object(o) => {
                if name == "spl_object_id" {
                    Value::Int(o.borrow().id as i64)
                } else {
                    Value::str(format!("{:032x}", o.borrow().id))
                }
            }
            _ => Value::Null,
        },

        // ----- misc -----
        "iterator_to_array" | "iterator_count" | "iterator_apply" => {
            let name_l = name.to_lowercase();
            match arg(args, 0) {
                Value::Array(a) => {
                    let mut out = PhpArray::new();
                    for (k, c) in a.borrow().iter() {
                        out.set(k.clone(), c.borrow().clone());
                    }
                    Value::Array(Rc::new(RefCell::new(out)))
                }
                Value::Object(o) => {
                    // Materialize via the Iterator protocol (Generator,
                    // IteratorAggregate, plain Iterator).
                    let items = it.yield_from_collect(&Value::Object(o))?;
                    if name_l == "iterator_count" {
                        return Ok(Some(Value::Int(items.len() as i64)));
                    }
                    let mut out = PhpArray::new();
                    // $preserve_keys (default true): duplicate int keys
                    // overwrite; false → append.
                    let preserve = args.get(1).map(|c| c.borrow().is_truthy()).unwrap_or(true);
                    for (k, v) in items {
                        let v = v.borrow().clone();
                        if preserve {
                            out.set(crate::value::to_key(&k), v);
                        } else {
                            out.push(v);
                        }
                    }
                    Value::Array(Rc::new(RefCell::new(out)))
                }
                _ => Value::Array(Rc::new(RefCell::new(PhpArray::new()))),
            }
        }
        "spl_autoload_register" => {
            if let Some(v) = args.first() {
                // ($callback, $throw, $prepend) — a truthy 3rd arg
                // prepends the loader (variance/loading_exception*).
                let prepend = args.get(2).map(|v| v.borrow().is_truthy()).unwrap_or(false);
                if prepend {
                    it.autoload_fns.insert(0, v.borrow().clone());
                } else {
                    it.autoload_fns.push(v.borrow().clone());
                }
            }
            Value::Bool(true)
        }
        "spl_autoload_unregister" => {
            // exact-value match — autoload lists are tiny in practice.
            if let Some(v) = args.first() {
                let target = v.borrow().clone();
                it.autoload_fns
                    .retain(|f| !crate::value::identical(f, &target));
            }
            Value::Bool(true)
        }
        "spl_autoload_functions" => {
            let mut a = PhpArray::new();
            for f in &it.autoload_fns {
                a.push(f.clone());
            }
            Value::Array(Rc::new(RefCell::new(a)))
        }
        "spl_autoload_call" => {
            let n = arg_str(it, args, 0);
            it.run_autoload(&n)?;
            Value::Bool(true)
        }
        "iterator_from_array" => Value::Null,
        _ => return Ok(None),
    }))
}
