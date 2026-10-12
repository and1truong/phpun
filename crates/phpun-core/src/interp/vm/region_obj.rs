//! Object foreach regions. Helpers complete a proven callback-free op or return
//! before side effects so dispatch can resume at that exact PC.
use super::native::region::{pair_of, ObjCtx, K_PTR};
use super::*;

impl Interp<'_> {
    pub(in crate::interp) unsafe extern "C" fn rh_line(ctx: *mut ObjCtx, pc: u32, _n: u32) -> u8 {
        let c = &mut *ctx;
        let comp = &*c.comp;
        let Op::Line(l) = &comp.ops[pc as usize] else {
            return 0;
        };
        let it = &mut *c.it;
        if comp.hybrid || comp.top_level {
            if let Some(flow) = it.statement_boundary() {
                c.flow = Some(flow);
                return 0;
            }
        }
        it.cur_line = *l;
        it.send_line = None;
        1
    }

    pub(in crate::interp) unsafe extern "C" fn rh_sweep(ctx: *mut ObjCtx, _pc: u32, _n: u32) -> u8 {
        let c = &mut *ctx;
        // Native ops do not create expression temporaries; refuse a callback.
        u8::from((*c.it).expr_temps.len() == c.tmark)
    }

    pub(in crate::interp) unsafe extern "C" fn rh_mcallslot(
        ctx: *mut ObjCtx,
        pc: u32,
        n: u32,
    ) -> u8 {
        let c = &mut *ctx;
        let comp = &*c.comp;
        let Op::MethodCallSlot {
            obj,
            name,
            site,
            cache,
            ..
        } = &comp.ops[pc as usize]
        else {
            return 0;
        };
        let it = &mut *c.it;
        let slots = std::slice::from_raw_parts(c.slots, c.nslots);
        if c.raw[*obj as usize][1] != K_PTR {
            return 0;
        }
        let Ok(Value::Object(o)) = it.vm_slot_value(comp, slots, *obj) else {
            return 0;
        };
        let Some((o, core)) = it.vm_method_target(o, name, cache, *site) else {
            return 0;
        };
        // The float-leaf proof returns before executing PHP on a miss.
        let Some(v) = it.vm_float_method(&core, &[], &o) else {
            return 0;
        };
        c.stack_put(n as usize, v);
        1
    }

    pub(in crate::interp) unsafe extern "C" fn rh_fenext(ctx: *mut ObjCtx, pc: u32, _n: u32) -> u8 {
        let c = &mut *ctx;
        let comp = &*c.comp;
        let Op::FeNext {
            idx, vslot, kslot, ..
        } = &comp.ops[pc as usize]
        else {
            return 0;
        };
        let it = &mut *c.it;
        // A purple-buffer overflow can execute PHP destructors during a store.
        // Do not consume the snapshot item until all callback gates pass.
        // Two displaced pinned objects can each purple-add twice.
        if it.gc_pending >= 9_996 && !it.gc_purpled.is_empty() {
            c.paused = true;
            return 0;
        }
        let slots = std::slice::from_raw_parts_mut(c.slots, c.nslots);
        for i in [*vslot, *kslot] {
            if i == u16::MAX || c.raw[i as usize][1] != K_PTR {
                continue;
            }
            let value = match &slots[i as usize] {
                Slot::V(v) => v,
                Slot::C(cell) => {
                    let v = cell.borrow();
                    if !matches!(&*v, Value::Object(o) if Rc::strong_count(o) > 1)
                        && !matches!(&*v, Value::Str(_))
                    {
                        return 0;
                    }
                    continue;
                }
                Slot::Arg(j) => &it.stack.last().unwrap().value_args[*j as usize],
                Slot::Uninit => continue,
            };
            if !matches!(value, Value::Object(o) if Rc::strong_count(o) > 1)
                && !matches!(value, Value::Str(_))
            {
                return 0;
            }
        }
        let states = &mut *c.fe_states;
        let next = states
            .get_mut(*idx as usize)
            .and_then(|s| s.as_mut())
            .and_then(|iter| iter.next());
        let Some((key, item)) = next else {
            return 2;
        };
        if *kslot != u16::MAX {
            if let Err(e) =
                it.fe_store_slot(comp, slots, *kslot, crate::interp::util::key_value(&key))
            {
                c.err = Some(e);
                return 0;
            }
            c.resync(*kslot as usize);
        }
        let value = match item {
            FeItem::Val(v) => v,
            FeItem::Cell(cell) => cell.borrow().clone(),
        };
        if let Err(e) = it.fe_store_slot(comp, slots, *vslot, value) {
            c.err = Some(e);
            return 0;
        }
        c.resync(*vslot as usize);
        1
    }

    #[allow(clippy::too_many_arguments, clippy::type_complexity)]
    pub(in crate::interp) fn vm_obj_region(
        &mut self,
        comp: &Compiled,
        slots: &mut [Slot],
        tmark: usize,
        fe_states: &mut Vec<Option<std::vec::IntoIter<(ArrKey, FeItem)>>>,
        cell: &std::cell::OnceCell<Option<(native::Program, Vec<Option<usize>>)>>,
        fails: &std::cell::Cell<u64>,
        start: usize,
        end: usize,
    ) -> Result<Option<(usize, Option<Flow>, Vec<Value>)>, PhpError> {
        if fails.get() >= 4 {
            return Ok(None);
        }
        // Cache unsupported bodies too, before allocating or scanning mirrors.
        let program = cell.get_or_init(|| native::Program::obj_region(self, comp, start, end));
        let Some((program, depths)) = program else {
            return Ok(None);
        };
        if slots.len() > 256
            || self.deadline.is_some()
            || Self::callprof_on()
            || coverage_on()
            || self.globals_arr.is_some()
            || self.in_handler
            || self.error_handler.is_some()
            || self.dim_throw.is_some()
            || self.pending_exception.is_some()
            || self.expr_temps.len() != tmark
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
            || self.detached_dim
        {
            return Ok(None);
        }
        let limit = self.memory_limit_bytes();
        if limit > 0 && self.mem_total().saturating_add(1 << 20) >= limit {
            return Ok(None);
        }
        let mut used = [0u64; 4];
        let mut written = [0u64; 4];
        for op in &comp.ops[start..end] {
            let mut u = |i: u16| used[i as usize / 64] |= 1 << (i as usize % 64);
            let mut w = |i: u16| {
                u(i);
                written[i as usize / 64] |= 1 << (i as usize % 64);
            };
            match op {
                Op::Load(i)
                | Op::BinaryCv(_, i)
                | Op::BinaryCvConst(_, i, _)
                | Op::MethodCallSlot { obj: i, .. } => u(*i),
                Op::JumpCmp { a, b, .. } => {
                    u(*a);
                    u(*b);
                }
                Op::Store(i)
                | Op::IncDec { slot: i, .. }
                | Op::IncJump { slot: i, .. }
                | Op::BinaryCvStore(_, i) => w(*i),
                Op::FeNext { vslot, kslot, .. } => {
                    w(*vslot);
                    if *kslot != u16::MAX {
                        w(*kslot);
                    }
                }
                _ => {}
            }
        }
        let mut ctx = ObjCtx {
            raw: [[0; 2]; 256],
            stack: [[0; 2]; 64],
            it: self,
            comp,
            slots: slots.as_mut_ptr(),
            nslots: slots.len(),
            fe_states,
            err: None,
            flow: None,
            paused: false,
            tmark,
            written,
        };
        let mut seen = std::collections::HashSet::new();
        for (i, slot) in slots.iter().enumerate() {
            if (used[i / 64] >> (i % 64)) & 1 == 0 {
                continue;
            }
            ctx.raw[i] = match slot {
                Slot::Uninit => [0, 0],
                Slot::V(v) => pair_of(v),
                Slot::C(cell) => {
                    let ptr = Rc::as_ptr(cell) as usize;
                    if self.is_ref_cell(cell)
                        || self.typed_slots.contains_key(&ptr)
                        || self.slot_owners.contains_key(&ptr)
                        || self.slot_merged.contains_key(&ptr)
                        || self.readonly_cells.contains_key(&ptr)
                        || !seen.insert(ptr)
                    {
                        return Ok(None);
                    }
                    pair_of(&cell.borrow())
                }
                Slot::Arg(j) => pair_of(&self.stack.last().unwrap().value_args[*j as usize]),
            };
        }
        let resume = program.run_obj_region(&mut ctx);
        // Only callback-free ops ran: canonical references cannot have changed
        // behind scalar mirrors. Fold before propagating a boundary failure.
        let folded = unsafe { ctx.flush() };
        if let Some(e) = ctx.err.take() {
            return Err(e);
        }
        folded?;
        if let Some(flow) = ctx.flow.take() {
            return Ok(Some((usize::MAX, Some(flow), Vec::new())));
        }
        if resume < end {
            if !ctx.paused {
                fails.set(fails.get() + 1);
            }
            if resume == start {
                // The canonical fetch must run now, not retry the same native
                // entry four times. Earlier completed iterations were folded.
                return Ok(None);
            }
            let depth = depths[resume].unwrap();
            let values = (0..depth).map(|n| ctx.stack_value(n)).collect();
            return Ok(Some((resume, None, values)));
        }
        program
            .executions
            .set(program.executions.get().saturating_add(1));
        Ok(Some((end, None, Vec::new())))
    }
}
