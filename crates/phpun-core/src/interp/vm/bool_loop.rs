//! Transactional typed execution of existing integer/boolean loop operations.
//! Failure discards private values and re-enters the unchanged VM region.

use super::{Compiled, Interp, Op, Slot, Value};
use crate::error::PhpError;
use std::rc::Rc;

#[derive(Clone, Copy)]
enum Scalar {
    Int(i64),
    Bool(bool),
}

impl Scalar {
    fn from_value(value: &Value) -> Option<Self> {
        match value {
            Value::Int(v) => Some(Self::Int(*v)),
            Value::Bool(v) => Some(Self::Bool(*v)),
            _ => None,
        }
    }
    fn truth(self) -> bool {
        match self {
            Self::Int(v) => v != 0,
            Self::Bool(v) => v,
        }
    }
    fn int(self) -> Option<i64> {
        match self {
            Self::Int(v) => Some(v),
            _ => None,
        }
    }
    fn value(self) -> Value {
        match self {
            Self::Int(v) => Value::Int(v),
            Self::Bool(v) => Value::Bool(v),
        }
    }
}

fn binary(op: &str, left: Scalar, right: Scalar) -> Option<Scalar> {
    let (a, b) = (left.int()?, right.int()?);
    Some(match op {
        "+" => Scalar::Int(a.checked_add(b)?),
        "-" => Scalar::Int(a.checked_sub(b)?),
        "*" => Scalar::Int(a.checked_mul(b)?),
        "%" => Scalar::Int(a.checked_rem(b)?),
        "<" => Scalar::Bool(a < b),
        "<=" => Scalar::Bool(a <= b),
        ">" => Scalar::Bool(a > b),
        ">=" => Scalar::Bool(a >= b),
        "==" | "===" => Scalar::Bool(a == b),
        "!=" | "!==" => Scalar::Bool(a != b),
        _ => return None,
    })
}

pub(super) fn eligible(ops: &[Op]) -> bool {
    ops.len() <= 512
        && ops.iter().any(|op| matches!(op, Op::DimCv { .. }))
        && ops.iter().all(|op| match op {
            Op::Const(Value::Int(_) | Value::Bool(_))
            | Op::Load(_)
            | Op::Store(_)
            | Op::Pop
            | Op::Binary(_)
            | Op::BinaryCv(..)
            | Op::BinaryCvConst(_, _, Value::Int(_))
            | Op::Not
            | Op::Boolify
            | Op::Jump(_)
            | Op::JumpIfFalse(_)
            | Op::JumpIfTrue(_)
            | Op::IncDec { .. }
            | Op::Line(_)
            | Op::Sweep
            | Op::BoolLoop { .. } => true,
            Op::DimCv { write, .. } => matches!(write, None | Some(Value::Bool(_))),
            _ => false,
        })
}

impl Interp<'_> {
    pub(super) fn vm_bool_loop(
        &mut self,
        comp: &Compiled,
        slots: &mut [Slot],
        start: usize,
        end: usize,
    ) -> Result<Option<usize>, PhpError> {
        // ponytail: bounded typed stack and one packed boolean table, not a
        // general register VM. Extend only when byte-gated measurements justify
        // more types/arrays; overflow, aliases and unsupported ops keep the VM.
        if slots.len() > 256
            || self.deadline.is_some()
            || Self::callprof_on()
            || super::coverage_on()
            || self.globals_arr.is_some()
            || self.in_handler
            || self.error_handler.is_some()
            || self.dim_throw.is_some()
            || self.pending_exception.is_some()
            || !self.expr_temps.is_empty()
            || !self.dim_key_conv.is_empty()
            || !self.dim_cv_bound.is_empty()
            || !self.dim_undef_cells.is_empty()
            || self.last_ret_cell.is_some()
            || self.last_prop_ov.is_some()
            || self.cast_err.is_some()
            || self.callable_probe_err.is_some()
            || crate::value::cmp_notices_pending()
            || self.mem_exceeded
            || self.oom_at.is_some()
            || !self.ob_stack.is_empty()
            || self.gc_pending != 0
        {
            return Ok(None);
        }
        let limit = self.memory_limit_bytes();
        if limit > 0 && self.mem_total().saturating_add(1 << 20) >= limit {
            return Ok(None);
        }
        let mut used = vec![false; slots.len()];
        let mut written = used.clone();
        let mut array_slot = None;
        for op in &comp.ops[start..end] {
            match op {
                Op::Load(i) | Op::BinaryCv(_, i) | Op::BinaryCvConst(_, i, _) => {
                    used[*i as usize] = true
                }
                Op::Store(i) | Op::IncDec { slot: i, .. } => {
                    used[*i as usize] = true;
                    written[*i as usize] = true;
                }
                Op::DimCv {
                    array, key_slot, ..
                } => {
                    if array_slot.is_some_and(|i| i != *array) {
                        return Ok(None);
                    }
                    array_slot = Some(*array);
                    if let Some(i) = key_slot {
                        used[*i as usize] = true;
                    }
                }
                _ => {}
            }
        }
        let Some(array_slot) = array_slot else {
            return Ok(None);
        };
        if used[array_slot as usize] {
            return Ok(None);
        }
        let protected = |c: &crate::value::Cell| {
            let ptr = Rc::as_ptr(c) as usize;
            self.is_ref_cell(c)
                || self.typed_slots.contains_key(&ptr)
                || self.slot_owners.contains_key(&ptr)
                || self.slot_merged.contains_key(&ptr)
                || self.readonly_cells.contains_key(&ptr)
        };
        let array = match &slots[array_slot as usize] {
            Slot::V(Value::Array(a)) => a.clone(),
            Slot::C(c) if !protected(c) => match &*c.borrow() {
                Value::Array(a) => a.clone(),
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
        // ponytail: at most 1MiB of speculative boolean storage. Larger tables
        // keep canonical execution; use guarded borrowed/register regions only
        // if measured large-table workloads justify removing this copy ceiling.
        if Rc::strong_count(&array) != 2
            || array.borrow().is_ref
            || array.borrow().entries.len() > 1 << 20
        {
            return Ok(None);
        }
        let Some(mut bools) = array.borrow().entries.packed_bools() else {
            return Ok(None);
        };
        let mut locals = vec![None; slots.len()];
        for (i, slot) in slots.iter().enumerate().filter(|(i, _)| used[*i]) {
            locals[i] = match slot {
                Slot::Uninit => None,
                Slot::V(v) => Scalar::from_value(v),
                Slot::C(c) if !protected(c) => Scalar::from_value(&c.borrow()),
                _ => return Ok(None),
            };
            if locals[i].is_none() && !matches!(slot, Slot::Uninit) {
                return Ok(None);
            }
        }
        #[cfg(all(target_arch = "x86_64", target_os = "linux"))]
        let native_line = {
            let Op::BoolLoop { native, .. } = &comp.ops[start - 1] else {
                unreachable!()
            };
            let program =
                native.get_or_init(|| super::native::Program::region(self, comp, start, end));
            if let Some(program) = program {
                // Explicit primitive ABI; never reinterpret a Value or Cell.
                let mut raw: Vec<[i64; 2]> = locals
                    .iter()
                    .map(|v| match v {
                        Some(Scalar::Int(v)) => [*v, 1],
                        Some(Scalar::Bool(v)) => [i64::from(*v), 2],
                        None => [0, 0],
                    })
                    .collect();
                let mut line = self.cur_line;
                if !program.run_region(&mut raw, &mut bools, &mut line) {
                    // Native failure may have changed private buffers; discard
                    // both and retry the original VM, never the modified copy.
                    return Ok(None);
                }
                for (local, [value, kind]) in locals.iter_mut().zip(raw) {
                    *local = match kind {
                        0 => None,
                        1 => Some(Scalar::Int(value)),
                        2 if value == 0 || value == 1 => Some(Scalar::Bool(value != 0)),
                        _ => return Ok(None),
                    };
                }
                Some(line)
            } else {
                None
            }
        };
        #[cfg(not(all(target_arch = "x86_64", target_os = "linux")))]
        let native_line: Option<usize> = None;
        let result = native_line.or_else(|| {
            let mut stack = Vec::with_capacity(16);
            let mut pc = start;
            let mut line = self.cur_line;
            // Bounded speculation: no infinite-loop/deadline behavior can be
            // hidden. A budget miss discards everything before canonical retry.
            for _ in 0..10_000_000 {
                if pc == end {
                    return stack.is_empty().then_some(line);
                }
                if pc < start || pc > end || stack.len() > 64 {
                    return None;
                }
                match &comp.ops[pc] {
                    Op::Const(v) => stack.push(Scalar::from_value(v)?),
                    Op::Load(i) => stack.push(locals[*i as usize]?),
                    Op::Store(i) => locals[*i as usize] = Some(*stack.last()?),
                    Op::Pop => {
                        stack.pop()?;
                    }
                    Op::Binary(op) => {
                        let right = stack.pop()?;
                        let left = stack.pop()?;
                        stack.push(binary(op, left, right)?);
                    }
                    Op::BinaryCv(op, i) => {
                        let right = stack.pop()?;
                        stack.push(binary(op, locals[*i as usize]?, right)?);
                    }
                    Op::BinaryCvConst(op, i, v) => {
                        stack.push(binary(op, locals[*i as usize]?, Scalar::from_value(v)?)?);
                    }
                    Op::Not => {
                        let v = stack.pop()?;
                        stack.push(Scalar::Bool(!v.truth()));
                    }
                    Op::Boolify => {
                        let v = stack.pop()?;
                        stack.push(Scalar::Bool(v.truth()));
                    }
                    Op::IncDec { slot, delta, post } => {
                        let old = locals[*slot as usize]?;
                        let new = Scalar::Int(old.int()?.checked_add(*delta)?);
                        locals[*slot as usize] = Some(new);
                        stack.push(if *post { old } else { new });
                    }
                    Op::DimCv {
                        key_slot,
                        key_int,
                        write,
                        ..
                    } => {
                        let key = match key_slot {
                            Some(i) => locals[*i as usize]?.int()?,
                            None => *key_int,
                        };
                        let value = bools.get_mut(usize::try_from(key).ok()?)?;
                        if let Some(Value::Bool(v)) = write {
                            *value = *v;
                        }
                        stack.push(Scalar::Bool(*value));
                    }
                    Op::Jump(target) => {
                        pc = *target;
                        continue;
                    }
                    Op::JumpIfFalse(target) | Op::JumpIfTrue(target) => {
                        let take =
                            stack.pop()?.truth() == matches!(&comp.ops[pc], Op::JumpIfTrue(_));
                        if take {
                            pc = *target;
                            continue;
                        }
                    }
                    Op::Line(v) => line = *v,
                    Op::Sweep | Op::BoolLoop { .. } => {}
                    _ => return None,
                }
                pc += 1;
            }
            None
        });
        let Some(line) = result else {
            return Ok(None);
        };
        // All writes are guarded scalar replacements; no callback ran while
        // executing the private buffers, so table/owner guards remain valid.
        array.borrow_mut().entries.replace_packed_bools(&bools);
        for i in 0..slots.len() {
            if !written[i] {
                continue;
            }
            let Some(value) = locals[i] else {
                continue;
            };
            match &mut slots[i] {
                Slot::C(c) => {
                    let c = c.clone();
                    self.cell_store(&c, value.value())?;
                }
                slot => *slot = Slot::V(value.value()),
            }
            self.vm_publish_global(comp, slots, i as u16);
        }
        Ok(Some(line))
    }
}
